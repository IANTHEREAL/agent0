//! JOIN query execution for the SQL executor

use super::core::Executor;
use super::super::helpers::{
    apply_offset_limit_fetch, collect_having_agg_funcs, dedup_rows,
    distinct_on_rows_join_with_indices, eval_having_expr_join, get_select_item_name,
    infer_expr_type, normalize_ident, AggExpr,
};
use super::super::information_schema::VirtualTableFilter;
use super::super::names;
use super::super::operators::{hash_row_key_for_join, row_key_has_null_for_join, row_keys_equal_for_join, HashJoinConfig};
use super::super::sequences;
use super::super::window::{compute_window_functions_join, extract_window_functions};
use super::super::{
    expr::{eval_expr_join, JoinContext},
    parse_sql, Aggregator, ExecuteResult,
};
use crate::types::{ColumnDef, DataType, Row, TableSchema, Value};
use anyhow::{anyhow, Result};
use chrono::TimeZone;
use sqlparser::ast::{
    BinaryOperator, Distinct, Expr, FunctionArg, FunctionArgExpr, GroupByExpr, Ident,
    JoinConstraint, JoinOperator, Query, SelectItem, Statement, TableFactor,
};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::future::Future;
use tikv_client::Transaction;

tokio::task_local! {
    static VIEW_EXPANSION_STACK: RefCell<Vec<String>>;
}

const MAX_VIEW_EXPANSION_DEPTH: usize = 64;

async fn with_view_expansion_stack<T>(future: impl Future<Output = T>) -> T {
    if VIEW_EXPANSION_STACK.try_with(|_| ()).is_ok() {
        future.await
    } else {
        VIEW_EXPANSION_STACK.scope(RefCell::new(Vec::new()), future).await
    }
}

struct ViewExpansionGuard {
    view_name: String,
}

impl ViewExpansionGuard {
    fn push(view_name: String) -> Result<Self> {
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

fn extract_virtual_table_filter(where_clause: &Option<Expr>) -> VirtualTableFilter {
    let mut filter = VirtualTableFilter::default();
    if let Some(expr) = where_clause {
        extract_filter_from_expr(expr, &mut filter);
    }
    filter
}

fn extract_filter_from_expr(expr: &Expr, filter: &mut VirtualTableFilter) {
    match expr {
        Expr::BinaryOp { left, op, right } => {
            if matches!(op, BinaryOperator::And) {
                extract_filter_from_expr(left, filter);
                extract_filter_from_expr(right, filter);
            } else if matches!(op, BinaryOperator::Eq) {
                if let (Some(col), Some(val)) = (get_column_name(left), get_string_value(right)) {
                    match col.to_lowercase().as_str() {
                        "table_name" | "relname" => filter.table_name = Some(val),
                        "table_schema" | "nspname" => filter.table_schema = Some(val),
                        _ => {}
                    }
                } else if let (Some(col), Some(val)) = (get_column_name(right), get_string_value(left)) {
                    match col.to_lowercase().as_str() {
                        "table_name" | "relname" => filter.table_name = Some(val),
                        "table_schema" | "nspname" => filter.table_schema = Some(val),
                        _ => {}
                    }
                }
            }
        }
        Expr::Nested(inner) => extract_filter_from_expr(inner, filter),
        _ => {}
    }
}

fn build_type_infer_schema_for_join(combined_schemas: &[(String, TableSchema)]) -> TableSchema {
    let mut columns: Vec<ColumnDef> = Vec::new();

    for (_, schema) in combined_schemas {
        columns.extend(schema.columns.clone());
    }

    for (alias, schema) in combined_schemas {
        for col in &schema.columns {
            let mut qualified = col.clone();
            qualified.name = format!("{}.{}", alias, col.name);
            columns.push(qualified);
        }
    }

    TableSchema {
        name: "joined_infer".to_string(),
        table_id: 0,
        columns,
        version: 1,
        pk_constraint_name: None,
        pk_indices: vec![],
        indexes: vec![],
        check_constraints: vec![],
        foreign_keys: vec![],
        owner: String::new(),
    }
}

fn project_wildcard_natural_join(
    combined_schemas: &[(String, TableSchema)],
    natural_join_common_cols: &[String],
    rows_to_project: Vec<Row>,
) -> (Vec<String>, Vec<DataType>, Vec<Row>) {
    let mut cols: Vec<String> = natural_join_common_cols.iter().cloned().collect();

    let mut common_offsets: HashMap<String, usize> = HashMap::new();
    let mut common_types: HashMap<String, DataType> = HashMap::new();

    let mut non_common_offsets: Vec<usize> = Vec::new();
    let mut non_common_types: Vec<DataType> = Vec::new();

    let mut offset = 0;
    for (_, schema) in combined_schemas {
        for col in &schema.columns {
            if natural_join_common_cols.contains(&col.name) {
                if !common_offsets.contains_key(&col.name) {
                    common_offsets.insert(col.name.clone(), offset);
                    common_types.insert(col.name.clone(), col.data_type.clone());
                }
            } else {
                cols.push(col.name.clone());
                non_common_types.push(col.data_type.clone());
                non_common_offsets.push(offset);
            }
            offset += 1;
        }
    }

    let mut column_types: Vec<DataType> = Vec::new();
    for common_col in natural_join_common_cols {
        column_types.push(
            common_types
                .get(common_col)
                .cloned()
                .unwrap_or(DataType::Text),
        );
    }
    column_types.extend(non_common_types);

    let mut col_indices_to_keep: Vec<Option<usize>> =
        Vec::with_capacity(natural_join_common_cols.len() + non_common_offsets.len());
    for common_col in natural_join_common_cols {
        col_indices_to_keep.push(common_offsets.get(common_col).copied());
    }
    col_indices_to_keep.extend(non_common_offsets.into_iter().map(Some));

    let result_rows = rows_to_project
        .into_iter()
        .map(|row| {
            let vals: Vec<Value> = col_indices_to_keep
                .iter()
                .map(|idx| {
                    idx.and_then(|idx| row.values.get(idx).cloned())
                        .unwrap_or(Value::Null)
                })
                .collect();
            Row::new(vals)
        })
        .collect();

    (cols, column_types, result_rows)
}

fn get_column_name(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Identifier(ident) => Some(ident.value.clone()),
        Expr::CompoundIdentifier(parts) => parts.last().map(|i| i.value.clone()),
        _ => None,
    }
}

fn get_string_value(expr: &Expr) -> Option<String> {
    match expr {
        Expr::Value(sqlparser::ast::Value::SingleQuotedString(s)) => Some(s.clone()),
        Expr::Value(sqlparser::ast::Value::DoubleQuotedString(s)) => Some(s.clone()),
        Expr::Function(func) => {
            let fn_name = func.name.to_string().to_uppercase();
            if fn_name == "CURRENT_SCHEMA" || fn_name == "CURRENT_SCHEMA()" {
                Some("public".to_string())
            } else {
                None
            }
        }
        _ => None,
    }
}

fn resolve_join_expr_offset(expr: &Expr, column_offsets: &HashMap<String, usize>) -> Option<usize> {
    match expr {
        Expr::Identifier(ident) => column_offsets.get(&ident.value).copied(),
        Expr::CompoundIdentifier(parts) => {
            // Match `table.column` and `schema.table.column` similarly to `eval_expr_join`.
            let (table_alias, col_name) = if parts.len() == 2 {
                (&parts[0].value, &parts[1].value)
            } else if parts.len() == 3 {
                (&parts[1].value, &parts[2].value)
            } else {
                return None;
            };

            let key = format!("{}.{}", table_alias, col_name);
            if let Some(&offset) = column_offsets.get(&key) {
                return Some(offset);
            }

            let key_lower = key.to_lowercase();
            for (k, &offset) in column_offsets {
                if k.to_lowercase() == key_lower {
                    return Some(offset);
                }
            }

            if table_alias.contains("->") {
                let suffix = format!(".{}.{}", table_alias, col_name).to_lowercase();
                for (k, &offset) in column_offsets {
                    if k.to_lowercase().ends_with(&suffix) {
                        return Some(offset);
                    }
                }

                let direct_suffix = format!("{}.{}", table_alias, col_name).to_lowercase();
                for (k, &offset) in column_offsets {
                    let k_lower = k.to_lowercase();
                    if k_lower == direct_suffix || k_lower.ends_with(&format!(".{}", direct_suffix)) {
                        return Some(offset);
                    }
                }
            }

            None
        }
        Expr::Nested(inner) => resolve_join_expr_offset(inner, column_offsets),
        _ => None,
    }
}

fn extract_equi_join_key_indices_for_hash_join(
    expr: &Expr,
    column_offsets: &HashMap<String, usize>,
    left_col_count: usize,
    right_col_count: usize,
) -> Option<(Vec<usize>, Vec<usize>)> {
    match expr {
        Expr::BinaryOp { left, op, right } => match op {
            BinaryOperator::And => {
                let (mut lk1, mut rk1) = extract_equi_join_key_indices_for_hash_join(
                    left,
                    column_offsets,
                    left_col_count,
                    right_col_count,
                )?;
                let (lk2, rk2) = extract_equi_join_key_indices_for_hash_join(
                    right,
                    column_offsets,
                    left_col_count,
                    right_col_count,
                )?;
                lk1.extend(lk2);
                rk1.extend(rk2);
                Some((lk1, rk1))
            }
            BinaryOperator::Eq => {
                let left_off = resolve_join_expr_offset(left, column_offsets)?;
                let right_off = resolve_join_expr_offset(right, column_offsets)?;

                let left_in_left = left_off < left_col_count;
                let right_in_left = right_off < left_col_count;

                let left_in_right = left_off >= left_col_count
                    && left_off < left_col_count.saturating_add(right_col_count);
                let right_in_right = right_off >= left_col_count
                    && right_off < left_col_count.saturating_add(right_col_count);

                // Require one side from the current left input and one side from the current right input.
                if left_in_left && right_in_right {
                    return Some((vec![left_off], vec![right_off - left_col_count]));
                }
                if right_in_left && left_in_right {
                    return Some((vec![right_off], vec![left_off - left_col_count]));
                }
                None
            }
            _ => None,
        },
        Expr::Nested(inner) => extract_equi_join_key_indices_for_hash_join(
            inner,
            column_offsets,
            left_col_count,
            right_col_count,
        ),
        _ => None,
    }
}

fn hash_join_key_types_compatible(left: &DataType, right: &DataType) -> bool {
    use DataType::*;
    match (left, right) {
        (Int32, Int32) | (Int64, Int64) | (Int32, Int64) | (Int64, Int32) => true,
        (Numeric { .. }, Numeric { .. }) => true,
        (Boolean, Boolean)
        | (Text, Text)
        | (Bytes, Bytes)
        | (Timestamp, Timestamp)
        | (Date, Date)
        | (Uuid, Uuid) => true,
        _ => false,
    }
}

impl Executor {
    #[allow(dead_code)]
    pub(crate) async fn execute_join_query(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        query: &Query,
        select: &sqlparser::ast::Select,
    ) -> Result<ExecuteResult> {
        self.execute_join_query_with_ctes(
            txn,
            db_id,
            sequence_values,
            search_path,
            query,
            select,
            &HashMap::new(),
        )
        .await
    }

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

        // Check if this is a known scalar function (with or without parentheses)
        let t_upper = table_name.trim_end_matches("()").to_uppercase();
        if t_upper == "_PGTIKV_SYS_OBSERVABILITY"
            || t_upper.ends_with("._PGTIKV_SYS_OBSERVABILITY")
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

        if t_upper == "_PGTIKV_SYS_TRIGGER_QUEUE_STATS"
            || t_upper.ends_with("._PGTIKV_SYS_TRIGGER_QUEUE_STATS")
        {
            use super::super::trigger_queue::{encode_trigger_dlq_prefix, encode_trigger_queue_prefix};
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
            };

            return Ok((schema, rows));
        }
        if matches!(
            t_upper.as_str(),
            "CURRENT_SCHEMA" | "CURRENT_DATABASE" | "CURRENT_USER" | "SESSION_USER" | "USER"
        ) {
            let result = match t_upper.as_str() {
                "CURRENT_SCHEMA" => Value::Text(names::default_schema(search_path).to_string()),
                "CURRENT_DATABASE" => Value::Text("testdb".to_string()),
                "CURRENT_USER" | "SESSION_USER" | "USER" => Value::Text("postgres".to_string()),
                _ => unreachable!(),
            };

            // Create a single-column, single-row result
            let col_name = t_upper.to_lowercase();
            let schema = TableSchema {
                table_id: 0,
                name: table_name.to_string(),
                columns: vec![ColumnDef {
                    name: col_name.clone(),
                    data_type: result.data_type().unwrap_or(DataType::Text),
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
            };
            let rows = vec![Row::new(vec![result])];
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
                        } => {
                            let inferred_types = column_types.unwrap_or_else(|| {
                                if let Some(first) = rows.first() {
                                    first
                                        .values
                                        .iter()
                                        .map(|v| v.data_type().unwrap_or(DataType::Text))
                                        .collect()
                                } else {
                                    vec![DataType::Text; columns.len()]
                                }
                            });

                            let schema = TableSchema {
                                table_id: 0,
                                name: candidate.clone(),
                                columns: columns
                                    .iter()
                                    .enumerate()
                                    .map(|(i, n)| ColumnDef {
                                        name: n.clone(),
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

        // Handle function calls in FROM clause (e.g., SELECT * FROM current_schema())
        if table_name.ends_with("()") || table_name.contains("(") && table_name.contains(")") {
            // Parse as a function call
            let func_name = table_name.trim_end_matches("()").to_uppercase();
            let result = match func_name.as_str() {
                "CURRENT_SCHEMA" => Value::Text(names::default_schema(search_path).to_string()),
                "CURRENT_DATABASE" => Value::Text("testdb".to_string()),
                "CURRENT_USER" | "SESSION_USER" | "USER" => Value::Text("postgres".to_string()),
                _ => return Err(anyhow!("Function '{}' not found", func_name)),
            };

            // Create a single-column, single-row result
            let col_name = func_name.to_lowercase();
            let schema = TableSchema {
                table_id: 0,
                name: table_name.to_string(),
                columns: vec![ColumnDef {
                    name: col_name.clone(),
                    data_type: result.data_type().unwrap_or(DataType::Text),
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
            };
            let rows = vec![Row::new(vec![result])];
            return Ok((schema, rows));
        }
        Err(anyhow!("Table '{}' not found", table_name))
    }

    pub(crate) async fn execute_generate_series(
        &self,
        args: &[FunctionArg],
        alias_name: &str,
        table_alias: Option<&sqlparser::ast::TableAlias>,
    ) -> Result<(TableSchema, Vec<Row>)> {
        use super::super::expr::eval_expr;

        fn extract_expr(arg: &FunctionArg) -> Result<&Expr> {
            match arg {
                FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => Ok(e),
                _ => Err(anyhow!("generate_series requires expression arguments")),
            }
        }

        if args.len() < 2 {
            return Err(anyhow!("generate_series requires at least 2 arguments"));
        }

        let start_val = eval_expr(extract_expr(&args[0])?, None, None)?;
        let stop_val = eval_expr(extract_expr(&args[1])?, None, None)?;
        let step_val = if args.len() >= 3 {
            eval_expr(extract_expr(&args[2])?, None, None)?
        } else {
            Value::Null
        };

        let (values, data_type) = generate_series_values(&start_val, &stop_val, &step_val)?;

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
        };

        let rows: Vec<Row> = values.into_iter().map(|v| Row::new(vec![v])).collect();
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
                self.execute_query_with_outer_ctes(txn, db_id, sequence_values, search_path, &q, ctes)
                    .await
            } else {
                Err(anyhow!("Invalid view query"))
            }
        })
    }

    pub(crate) fn execute_derived_table<'a>(
        &'a self,
        txn: &'a mut Transaction,
        db_id: u64,
        sequence_values: &'a mut HashMap<String, i64>,
        search_path: &'a [String],
        subquery: &'a Query,
        alias: &'a str,
        alias_columns: &'a [Ident],
        ctes: &'a HashMap<String, (TableSchema, Vec<Row>)>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<(TableSchema, Vec<Row>)>> + Send + 'a>,
    > {
        Box::pin(async move {
            let result = self
                .execute_query_with_outer_ctes(txn, db_id, sequence_values, search_path, subquery, ctes)
                .await?;
            match result {
                ExecuteResult::Select {
                    columns,
                    column_types,
                    rows,
                } => {
                    let column_names = if alias_columns.is_empty() {
                        columns
                    } else if alias_columns.len() != columns.len() {
                        return Err(anyhow!(
                            "Derived table alias column count mismatch: expected {}, got {}",
                            columns.len(),
                            alias_columns.len()
                        ));
                    } else {
                        alias_columns.iter().map(normalize_ident).collect()
                    };

                    let inferred_types = column_types.unwrap_or_else(|| {
                        if let Some(first) = rows.first() {
                            first
                                .values
                                .iter()
                                .map(|v| v.data_type().unwrap_or(DataType::Text))
                                .collect()
                        } else {
                            vec![DataType::Text; column_names.len()]
                        }
                    });

                    let schema = TableSchema {
                        table_id: 0,
                        name: alias.to_string(),
                        columns: column_names
                            .iter()
                            .enumerate()
                            .map(|(i, n)| ColumnDef {
                                name: n.clone(),
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
                    };
                    Ok((schema, rows))
                }
                _ => Err(anyhow!("Derived table must return SELECT result")),
            }
        })
    }

    pub(crate) fn resolve_table_factor<'a>(
        &'a self,
        txn: &'a mut Transaction,
        db_id: u64,
        sequence_values: &'a mut HashMap<String, i64>,
        search_path: &'a [String],
        factor: &'a TableFactor,
        ctes: &'a HashMap<String, (TableSchema, Vec<Row>)>,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<(String, TableSchema, Vec<Row>)>> + Send + 'a>,
    > {
        let default_filter = VirtualTableFilter::default();
        self.resolve_table_factor_impl(
            txn,
            db_id,
            sequence_values,
            search_path,
            factor,
            ctes,
            default_filter,
        )
    }

    pub(crate) fn resolve_table_factor_filtered<'a>(
        &'a self,
        txn: &'a mut Transaction,
        db_id: u64,
        sequence_values: &'a mut HashMap<String, i64>,
        search_path: &'a [String],
        factor: &'a TableFactor,
        ctes: &'a HashMap<String, (TableSchema, Vec<Row>)>,
        filter: &'a VirtualTableFilter,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<(String, TableSchema, Vec<Row>)>> + Send + 'a>,
    > {
        self.resolve_table_factor_impl(
            txn,
            db_id,
            sequence_values,
            search_path,
            factor,
            ctes,
            filter.clone(),
        )
    }

    fn resolve_table_factor_impl<'a>(
        &'a self,
        txn: &'a mut Transaction,
        db_id: u64,
        sequence_values: &'a mut HashMap<String, i64>,
        search_path: &'a [String],
        factor: &'a TableFactor,
        ctes: &'a HashMap<String, (TableSchema, Vec<Row>)>,
        filter: VirtualTableFilter,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<(String, TableSchema, Vec<Row>)>> + Send + 'a>,
    > {
        Box::pin(async move {
            match factor {
                TableFactor::Table {
                    name, alias, args, ..
                } => {
                    let (schema_opt, obj_name) = names::split_object_name(name)?;
                    let als = alias
                        .as_ref()
                        .map(|a| a.name.value.clone())
                        .unwrap_or_else(|| obj_name.clone());
                    let tbl_upper = obj_name.to_uppercase();

                    if tbl_upper == "GENERATE_SERIES" {
                        if let Some(func_args) = args {
                            let (schema, rows) = self
                                .execute_generate_series(func_args, &als, alias.as_ref())
                                .await?;
                            return Ok((als, schema, rows));
                        } else {
                            return Err(anyhow!("generate_series requires at least 2 arguments"));
                        }
                    }

                    if let Some(func_args) = args {
                        if let Some((schema, rows)) = self
                            .try_execute_extension_table_function(
                                txn,
                                db_id,
                                search_path,
                                name,
                                func_args,
                                alias.as_ref(),
                            )
                            .await?
                        {
                            return Ok((als, schema, rows));
                        }
                    }

                    let is_scalar_function = matches!(
                        tbl_upper.as_str(),
                        "CURRENT_SCHEMA"
                            | "CURRENT_DATABASE"
                            | "CURRENT_USER"
                            | "SESSION_USER"
                            | "USER"
                    );

                    let table_name = if is_scalar_function {
                        format!("{}()", obj_name)
                    } else {
                        match schema_opt {
                            Some(schema) => format!("{}.{}", schema, obj_name),
                            None => obj_name,
                        }
                    };
                    let (schema, rows) = self
                        .get_table_data_filtered(
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            &table_name,
                            ctes,
                            &filter,
                        )
                        .await?;
                    Ok((als, schema, rows))
                }
                TableFactor::Derived {
                    subquery, alias, ..
                } => {
                    let alias_name = alias
                        .as_ref()
                        .map(|a| a.name.value.clone())
                        .unwrap_or_else(|| "subquery".to_string());
                    let alias_columns = alias.as_ref().map(|a| a.columns.as_slice()).unwrap_or(&[]);
                    let (schema, rows) = self
                        .execute_derived_table(
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            subquery,
                            &alias_name,
                            alias_columns,
                            ctes,
                        )
                        .await?;
                    Ok((alias_name, schema, rows))
                }
                TableFactor::NestedJoin {
                    table_with_joins,
                    alias,
                } => {
                    let (base_alias, base_schema, mut combined_rows) = self
                        .resolve_table_factor(
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            &table_with_joins.relation,
                            ctes,
                        )
                        .await?;

                    let mut all_table_aliases: Vec<(String, Vec<ColumnDef>)> =
                        vec![(base_alias.clone(), base_schema.columns.clone())];

                    let mut combined_schema = base_schema;

                    for join in &table_with_joins.joins {
                        let (join_alias, join_schema, join_rows) = self
                            .resolve_table_factor(
                                txn,
                                db_id,
                                sequence_values,
                                search_path,
                                &join.relation,
                                ctes,
                            )
                            .await?;

                        let left_columns: Vec<String> = all_table_aliases
                            .iter()
                            .flat_map(|(_, cols)| cols.iter().map(|c| c.name.clone()))
                            .collect();

                        let join_condition = match &join.join_operator {
                            JoinOperator::Inner(JoinConstraint::On(expr)) => Some(expr.clone()),
                            JoinOperator::LeftOuter(JoinConstraint::On(expr)) => Some(expr.clone()),
                            JoinOperator::RightOuter(JoinConstraint::On(expr)) => {
                                Some(expr.clone())
                            }
                            JoinOperator::FullOuter(JoinConstraint::On(expr)) => Some(expr.clone()),
                            JoinOperator::CrossJoin => None,
                            JoinOperator::Inner(JoinConstraint::None) => None,
                            JoinOperator::Inner(JoinConstraint::Using(cols))
                            | JoinOperator::LeftOuter(JoinConstraint::Using(cols))
                            | JoinOperator::RightOuter(JoinConstraint::Using(cols))
                            | JoinOperator::FullOuter(JoinConstraint::Using(cols)) => {
                                let using_cols: Vec<String> =
                                    cols.iter().map(|c| normalize_ident(c)).collect();
                                if using_cols.is_empty() {
                                    None
                                } else {
                                    let left_alias = all_table_aliases
                                        .iter()
                                        .find(|(_, cols)| {
                                            cols.iter()
                                                .any(|c| c.name.eq_ignore_ascii_case(&using_cols[0]))
                                        })
                                        .map(|(a, _)| a.clone())
                                        .unwrap_or_else(|| base_alias.clone());
                                    let cond = using_cols
                                        .iter()
                                        .map(|col| Expr::BinaryOp {
                                            left: Box::new(Expr::CompoundIdentifier(vec![
                                                Ident::new(left_alias.clone()),
                                                Ident::new(col.clone()),
                                            ])),
                                            op: BinaryOperator::Eq,
                                            right: Box::new(Expr::CompoundIdentifier(vec![
                                                Ident::new(join_alias.clone()),
                                                Ident::new(col.clone()),
                                            ])),
                                        })
                                        .reduce(|a, b| Expr::BinaryOp {
                                            left: Box::new(a),
                                            op: BinaryOperator::And,
                                            right: Box::new(b),
                                        });
                                    cond
                                }
                            }
                            JoinOperator::Inner(JoinConstraint::Natural)
                            | JoinOperator::LeftOuter(JoinConstraint::Natural)
                            | JoinOperator::RightOuter(JoinConstraint::Natural)
                            | JoinOperator::FullOuter(JoinConstraint::Natural) => {
                                let right_columns: Vec<String> = join_schema
                                    .columns
                                    .iter()
                                    .map(|c| c.name.clone())
                                    .collect();
                                let common_cols: Vec<String> = left_columns
                                    .iter()
                                    .filter(|c| right_columns.contains(c))
                                    .cloned()
                                    .collect();
                                if common_cols.is_empty() {
                                    None
                                } else {
                                    let cond = common_cols
                                        .iter()
                                        .map(|col| Expr::BinaryOp {
                                            left: Box::new(Expr::CompoundIdentifier(vec![
                                                Ident::new(base_alias.clone()),
                                                Ident::new(col.clone()),
                                            ])),
                                            op: BinaryOperator::Eq,
                                            right: Box::new(Expr::CompoundIdentifier(vec![
                                                Ident::new(join_alias.clone()),
                                                Ident::new(col.clone()),
                                            ])),
                                        })
                                        .reduce(|a, b| Expr::BinaryOp {
                                            left: Box::new(a),
                                            op: BinaryOperator::And,
                                            right: Box::new(b),
                                        });
                                    cond
                                }
                            }
                            _ => None,
                        };

                        let is_left_join = matches!(
                            join.join_operator,
                            JoinOperator::LeftOuter(_) | JoinOperator::FullOuter(_)
                        );
                        let is_right_join = matches!(
                            join.join_operator,
                            JoinOperator::RightOuter(_) | JoinOperator::FullOuter(_)
                        );

                        let mut column_offsets: HashMap<String, usize> = HashMap::new();
                        let mut offset = 0;
                        for (tbl_alias, cols) in &all_table_aliases {
                            for col in cols {
                                column_offsets
                                    .insert(format!("{}.{}", tbl_alias, col.name), offset);
                                if !column_offsets.contains_key(&col.name) {
                                    column_offsets.insert(col.name.clone(), offset);
                                }
                                offset += 1;
                            }
                        }
                        for col in &join_schema.columns {
                            column_offsets.insert(format!("{}.{}", join_alias, col.name), offset);
                            if !column_offsets.contains_key(&col.name) {
                                column_offsets.insert(col.name.clone(), offset);
                            }
                            offset += 1;
                        }

                        let mut temp_columns = combined_schema.columns.clone();
                        temp_columns.extend(join_schema.columns.clone());
                        let temp_combined_schema = TableSchema {
                            table_id: 0,
                            name: "nested_join".to_string(),
                            columns: temp_columns,
                            pk_constraint_name: None,
                            pk_indices: vec![],
                            indexes: vec![],
                            version: 1,
                            check_constraints: vec![],
                            foreign_keys: vec![],
                            owner: String::new(),
                        };

                        let mut new_rows = Vec::new();
                        let mut right_matched_flags = vec![false; join_rows.len()];
                        let right_cols = join_schema.columns.len();

                        for left_row in &combined_rows {
                            let mut matched_any = false;
                            for (right_idx, right_row) in join_rows.iter().enumerate() {
                                let mut combined_values = left_row.values.clone();
                                combined_values.extend(right_row.values.clone());
                                let combined_row = Row::new(combined_values);

                                let matches = if let Some(cond) = &join_condition {
                                    let ctx = JoinContext {
                                        tables: HashMap::new(),
                                        column_offsets: column_offsets.clone(),
                                        combined_row: &combined_row,
                                        combined_schema: &temp_combined_schema,
                                    };
                                    matches!(
                                        self.eval_expr_join_maybe_sequence(
                                            txn,
                                            db_id,
                                            sequence_values,
                                            search_path,
                                            cond,
                                            &ctx
                                        )
                                        .await?,
                                        Value::Boolean(true)
                                    )
                                } else {
                                    true
                                };

                                if matches {
                                    new_rows.push(combined_row);
                                    matched_any = true;
                                    right_matched_flags[right_idx] = true;
                                }
                            }

                            if !matched_any && is_left_join {
                                let mut combined_values = left_row.values.clone();
                                combined_values.extend(vec![Value::Null; right_cols]);
                                new_rows.push(Row::new(combined_values));
                            }
                        }

                        if is_right_join {
                            let left_cols = combined_schema.columns.len();
                            for (right_idx, right_row) in join_rows.iter().enumerate() {
                                if !right_matched_flags[right_idx] {
                                    let mut combined_values = vec![Value::Null; left_cols];
                                    combined_values.extend(right_row.values.clone());
                                    new_rows.push(Row::new(combined_values));
                                }
                            }
                        }

                        all_table_aliases.push((join_alias.clone(), join_schema.columns.clone()));

                        let mut new_columns = combined_schema.columns.clone();
                        new_columns.extend(join_schema.columns.clone());
                        combined_schema = TableSchema {
                            table_id: 0,
                            name: "nested_join".to_string(),
                            columns: new_columns,
                            pk_constraint_name: None,
                            pk_indices: vec![],
                            indexes: vec![],
                            version: 1,
                            check_constraints: vec![],
                            foreign_keys: vec![],
                            owner: String::new(),
                        };
                        combined_rows = new_rows;
                    }

                    let final_alias =
                        alias
                            .as_ref()
                            .map(|a| a.name.value.clone())
                            .unwrap_or_else(|| {
                                all_table_aliases
                                    .first()
                                    .map(|(a, _)| a.clone())
                                    .unwrap_or_default()
                            });

                    let mut prefixed_columns: Vec<ColumnDef> = Vec::new();
                    for (tbl_alias, cols) in &all_table_aliases {
                        for col in cols {
                            let mut new_col = col.clone();
                            new_col.name = format!("{}.{}", tbl_alias, col.name);
                            prefixed_columns.push(new_col);
                        }
                    }

                    let final_schema = TableSchema {
                        table_id: 0,
                        name: final_alias.clone(),
                        columns: prefixed_columns,
                        pk_constraint_name: None,
                        pk_indices: vec![],
                        indexes: vec![],
                        version: 1,
                        check_constraints: vec![],
                        foreign_keys: vec![],
                        owner: String::new(),
                    };

                    Ok((final_alias, final_schema, combined_rows))
                }
                _ => Err(anyhow!("Unsupported table factor")),
            }
        })
    }

    pub(crate) async fn execute_join_query_with_ctes(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        query: &Query,
        select: &sqlparser::ast::Select,
        ctes: &HashMap<String, (TableSchema, Vec<Row>)>,
    ) -> Result<ExecuteResult> {
        let vt_filter = extract_virtual_table_filter(&select.selection);

        let (base_alias, base_schema, base_rows) = self
            .resolve_table_factor_filtered(
                txn,
                db_id,
                sequence_values,
                search_path,
                &select.from[0].relation,
                ctes,
                &vt_filter,
            )
            .await?;

        let mut combined_schemas: Vec<(String, TableSchema)> =
            vec![(base_alias.clone(), base_schema.clone())];
        let mut combined_rows: Vec<Row> = base_rows;
        let mut has_natural_join = false;
        let mut natural_join_common_cols: Vec<String> = Vec::new();

        let mut extra_from_items: Vec<(Vec<(String, TableSchema)>, Vec<Row>)> = Vec::new();

        for from_item in select.from.iter().skip(1) {
            let (extra_alias, extra_schema, extra_rows) = match &from_item.relation {
                TableFactor::Table {
                    name, alias, args, ..
                } => {
                    let (schema_opt, obj_name) = names::split_object_name(name)?;
                    let als = alias
                        .as_ref()
                        .map(|a| a.name.value.clone())
                        .unwrap_or_else(|| obj_name.clone());
                    let tbl_upper = obj_name.to_uppercase();

                    if tbl_upper == "GENERATE_SERIES" {
                        if let Some(func_args) = args {
                            let (schema, rows) = self
                                .execute_generate_series(func_args, &als, alias.as_ref())
                                .await?;
                            (als, schema, rows)
                        } else {
                            return Err(anyhow!("generate_series requires at least 2 arguments"));
                        }
                    } else {
                        let tbl = match schema_opt {
                            Some(schema) => format!("{}.{}", schema, obj_name),
                            None => obj_name.clone(),
                        };
                        let (schema, rows) = self
                            .get_table_data_filtered(
                                txn,
                                db_id,
                                sequence_values,
                                search_path,
                                &tbl,
                                ctes,
                                &vt_filter,
                            )
                            .await?;
                        (als, schema, rows)
                    }
                }
                TableFactor::Derived {
                    subquery, alias, ..
                } => {
                    let alias_name = alias
                        .as_ref()
                        .map(|a| a.name.value.clone())
                        .unwrap_or_else(|| "subquery".to_string());
                    let alias_columns = alias.as_ref().map(|a| a.columns.as_slice()).unwrap_or(&[]);
                    let (schema, rows) = self
                        .execute_derived_table(
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            subquery,
                            &alias_name,
                            alias_columns,
                            ctes,
                        )
                        .await?;
                    (alias_name, schema, rows)
                }
                TableFactor::NestedJoin { .. } => {
                    self.resolve_table_factor_filtered(
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        &from_item.relation,
                        ctes,
                        &vt_filter,
                    )
                    .await?
                }
                _ => return Err(anyhow!("Unsupported table factor in FROM")),
            };

            let mut item_schemas: Vec<(String, TableSchema)> =
                vec![(extra_alias.clone(), extra_schema)];
            let mut item_rows: Vec<Row> = extra_rows;

            for extra_join in &from_item.joins {
                let (join_alias, join_schema, join_rows) = match &extra_join.relation {
                    TableFactor::Table {
                        name, alias, args, ..
                    } => {
                        let (schema_opt, obj_name) = names::split_object_name(name)?;
                        let als = alias
                            .as_ref()
                            .map(|a| a.name.value.clone())
                            .unwrap_or_else(|| obj_name.clone());
                        let tbl_upper = obj_name.to_uppercase();

                        if tbl_upper == "GENERATE_SERIES" {
                            if let Some(func_args) = args {
                                let (schema, rows) = self
                                    .execute_generate_series(func_args, &als, alias.as_ref())
                                    .await?;
                                (als, schema, rows)
                            } else {
                                return Err(anyhow!(
                                    "generate_series requires at least 2 arguments"
                                ));
                            }
                        } else {
                            let tbl = match schema_opt {
                                Some(schema) => format!("{}.{}", schema, obj_name),
                                None => obj_name.clone(),
                            };
                            let (schema, rows) = self
                                .get_table_data_filtered(
                                    txn,
                                    db_id,
                                    sequence_values,
                                    search_path,
                                    &tbl,
                                    ctes,
                                    &vt_filter,
                                )
                                .await?;
                            (als, schema, rows)
                        }
                    }
                    TableFactor::Derived {
                        subquery, alias, ..
                    } => {
                        let alias_name = alias
                            .as_ref()
                            .map(|a| a.name.value.clone())
                            .unwrap_or_else(|| "subquery".to_string());
                        let alias_columns =
                            alias.as_ref().map(|a| a.columns.as_slice()).unwrap_or(&[]);
                        let (schema, rows) = self
                            .execute_derived_table(
                                txn,
                                db_id,
                                sequence_values,
                                search_path,
                                subquery,
                                &alias_name,
                                alias_columns,
                                ctes,
                            )
                            .await?;
                        (alias_name, schema, rows)
                    }
                    TableFactor::NestedJoin { .. } => {
                        self.resolve_table_factor_filtered(
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            &extra_join.relation,
                            ctes,
                            &vt_filter,
                        )
                        .await?
                    }
                    _ => return Err(anyhow!("Unsupported join table factor")),
                };

                let left_columns: Vec<String> = item_schemas
                    .iter()
                    .flat_map(|(_, s)| s.columns.iter().map(|c| c.name.clone()))
                    .collect();
                let right_columns: Vec<String> =
                    join_schema.columns.iter().map(|c| c.name.clone()).collect();

                let (join_condition, is_natural) = match &extra_join.join_operator {
                    JoinOperator::Inner(JoinConstraint::On(expr)) => (Some(expr.clone()), false),
                    JoinOperator::LeftOuter(JoinConstraint::On(expr)) => (Some(expr.clone()), false),
                    JoinOperator::RightOuter(JoinConstraint::On(expr)) => (Some(expr.clone()), false),
                    JoinOperator::FullOuter(JoinConstraint::On(expr)) => (Some(expr.clone()), false),
                    JoinOperator::Inner(JoinConstraint::Natural)
                    | JoinOperator::LeftOuter(JoinConstraint::Natural)
                    | JoinOperator::RightOuter(JoinConstraint::Natural)
                    | JoinOperator::FullOuter(JoinConstraint::Natural) => {
                        let common_cols: Vec<String> = left_columns
                            .iter()
                            .filter(|c| right_columns.contains(c))
                            .cloned()
                            .collect();
                        has_natural_join = true;
                        natural_join_common_cols = common_cols.clone();
                        if common_cols.is_empty() {
                            (None, true)
                        } else {
                            let cond = common_cols
                                .iter()
                                .map(|col| Expr::BinaryOp {
                                    left: Box::new(Expr::CompoundIdentifier(vec![
                                        Ident::new(
                                            item_schemas
                                                .iter()
                                                .find(|(_, s)| {
                                                    s.columns
                                                        .iter()
                                                        .any(|c| c.name.eq_ignore_ascii_case(col))
                                                })
                                                .map(|(a, _)| a.clone())
                                                .unwrap_or_else(|| extra_alias.clone()),
                                        ),
                                        Ident::new(col.clone()),
                                    ])),
                                    op: BinaryOperator::Eq,
                                    right: Box::new(Expr::CompoundIdentifier(vec![
                                        Ident::new(join_alias.clone()),
                                        Ident::new(col.clone()),
                                    ])),
                                })
                                .reduce(|a, b| Expr::BinaryOp {
                                    left: Box::new(a),
                                    op: BinaryOperator::And,
                                    right: Box::new(b),
                                })
                                .unwrap();
                            (Some(cond), true)
                        }
                    }
                    JoinOperator::Inner(JoinConstraint::Using(cols))
                    | JoinOperator::LeftOuter(JoinConstraint::Using(cols))
                    | JoinOperator::RightOuter(JoinConstraint::Using(cols))
                    | JoinOperator::FullOuter(JoinConstraint::Using(cols)) => {
                        let using_cols: Vec<String> =
                            cols.iter().map(|c| normalize_ident(c)).collect();
                        has_natural_join = true;
                        natural_join_common_cols = using_cols.clone();
                        if using_cols.is_empty() {
                            (None, true)
                        } else {
                            let left_alias = item_schemas
                                .iter()
                                .find(|(_, s)| {
                                    s.columns
                                        .iter()
                                        .any(|c| c.name.eq_ignore_ascii_case(&using_cols[0]))
                                })
                                .map(|(a, _)| a.clone())
                                .unwrap_or_else(|| extra_alias.clone());
                            let cond = using_cols
                                .iter()
                                .map(|col| Expr::BinaryOp {
                                    left: Box::new(Expr::CompoundIdentifier(vec![
                                        Ident::new(left_alias.clone()),
                                        Ident::new(col.clone()),
                                    ])),
                                    op: BinaryOperator::Eq,
                                    right: Box::new(Expr::CompoundIdentifier(vec![
                                        Ident::new(join_alias.clone()),
                                        Ident::new(col.clone()),
                                    ])),
                                })
                                .reduce(|a, b| Expr::BinaryOp {
                                    left: Box::new(a),
                                    op: BinaryOperator::And,
                                    right: Box::new(b),
                                })
                                .unwrap();
                            (Some(cond), true)
                        }
                    }
                    JoinOperator::CrossJoin => (None, false),
                    JoinOperator::Inner(JoinConstraint::None) => (None, false),
                    _ => return Err(anyhow!("Unsupported JOIN type")),
                };
                let _ = is_natural;

                let has_correlated_subquery = join_condition.as_ref().map_or(false, |cond| {
                    item_schemas.iter().any(|(alias, _)| {
                        super::super::helpers::query_has_outer_reference_in_expr(cond, alias)
                    })
                });

                let join_condition = if let Some(cond) = join_condition {
                    if has_correlated_subquery {
                        Some(cond)
                    } else {
                        Some(
                            self.resolve_subqueries(
                                txn,
                                db_id,
                                sequence_values,
                                search_path,
                                &cond,
                                ctes,
                            )
                            .await?,
                        )
                    }
                } else {
                    None
                };

                let is_left_join =
                    matches!(&extra_join.join_operator, JoinOperator::LeftOuter(_));
                let is_right_join =
                    matches!(&extra_join.join_operator, JoinOperator::RightOuter(_));
                let is_full_join =
                    matches!(&extra_join.join_operator, JoinOperator::FullOuter(_));

                let left_col_count: usize =
                    item_schemas.iter().map(|(_, s)| s.columns.len()).sum();

                let mut column_offsets: HashMap<String, usize> = HashMap::new();
                let mut offset = 0;
                for (alias, schema) in &item_schemas {
                    for col in &schema.columns {
                        column_offsets.insert(format!("{}.{}", alias, col.name), offset);
                        if !column_offsets.contains_key(&col.name) {
                            column_offsets.insert(col.name.clone(), offset);
                        }
                        offset += 1;
                    }
                }
                for col in &join_schema.columns {
                    column_offsets.insert(format!("{}.{}", join_alias, col.name), offset);
                    if !column_offsets.contains_key(&col.name) {
                        column_offsets.insert(col.name.clone(), offset);
                    }
                    if col.name.contains('.') {
                        column_offsets.insert(col.name.clone(), offset);
                    }
                    offset += 1;
                }

                let mut combined_col_defs: Vec<ColumnDef> = Vec::new();
                for (_, schema) in &item_schemas {
                    combined_col_defs.extend(schema.columns.clone());
                }
                combined_col_defs.extend(join_schema.columns.clone());
                let temp_combined_schema = TableSchema {
                    name: "joined".to_string(),
                    table_id: 0,
                    columns: combined_col_defs,
                    version: 1,
                    pk_constraint_name: None,
                    pk_indices: vec![],
                    indexes: vec![],
                    check_constraints: vec![],
                    foreign_keys: vec![],
                    owner: String::new(),
                };

                let mut new_item_rows = Vec::new();
                let mut right_matched: Vec<bool> = vec![false; join_rows.len()];

                for left_row in &item_rows {
                    let mut matched = false;

                    let resolved_condition = if has_correlated_subquery {
                        if let Some(ref cond) = join_condition {
                            let mut substituted = cond.clone();
                            let mut value_offset = 0;
                            for (alias, schema) in &item_schemas {
                                let row_values: Vec<Value> = left_row.values
                                    [value_offset..value_offset + schema.columns.len()]
                                    .to_vec();
                                let outer_row = Row::new(row_values);
                                substituted = super::super::helpers::substitute_outer_values(
                                    &substituted,
                                    alias,
                                    schema,
                                    &outer_row,
                                );
                                value_offset += schema.columns.len();
                            }
                            Some(
                                self.resolve_subqueries(
                                    txn,
                                    db_id,
                                    sequence_values,
                                    search_path,
                                    &substituted,
                                    ctes,
                                )
                                .await?,
                            )
                        } else {
                            None
                        }
                    } else {
                        join_condition.clone()
                    };

                    for (right_idx, right_row) in join_rows.iter().enumerate() {
                        let mut combined_values = Vec::with_capacity(
                            left_row.values.len() + right_row.values.len(),
                        );
                        combined_values.extend(left_row.values.iter().cloned());
                        combined_values.extend(right_row.values.iter().cloned());
                        let combined_row = Row::new(combined_values);

                        let matches = if let Some(ref cond) = resolved_condition {
                            let ctx = JoinContext {
                                tables: HashMap::new(),
                                column_offsets: column_offsets.clone(),
                                combined_row: &combined_row,
                                combined_schema: &temp_combined_schema,
                            };
                            matches!(
                                self.eval_expr_join_maybe_sequence(
                                    txn,
                                    db_id,
                                    sequence_values,
                                    search_path,
                                    cond,
                                    &ctx
                                )
                                .await?,
                                Value::Boolean(true)
                            )
                        } else {
                            true
                        };

                        if matches {
                            new_item_rows.push(combined_row);
                            matched = true;
                            right_matched[right_idx] = true;
                        }
                    }

                    if (is_left_join || is_full_join) && !matched {
                        let mut combined_values = Vec::with_capacity(
                            left_row.values.len() + join_schema.columns.len(),
                        );
                        combined_values.extend(left_row.values.iter().cloned());
                        combined_values.extend(
                            std::iter::repeat(Value::Null).take(join_schema.columns.len()),
                        );
                        new_item_rows.push(Row::new(combined_values));
                    }
                }

                if is_right_join || is_full_join {
                    for (right_idx, right_row) in join_rows.iter().enumerate() {
                        if !right_matched[right_idx] {
                            let mut combined_values: Vec<Value> =
                                Vec::with_capacity(left_col_count + right_row.values.len());
                            combined_values
                                .extend(std::iter::repeat(Value::Null).take(left_col_count));
                            combined_values.extend(right_row.values.iter().cloned());
                            new_item_rows.push(Row::new(combined_values));
                        }
                    }
                }

                item_schemas.push((join_alias, join_schema));
                item_rows = new_item_rows;
            }

            extra_from_items.push((item_schemas, item_rows));
        }

        for join in &select.from[0].joins {
            if let TableFactor::Derived {
                lateral: true,
                subquery,
                alias,
                ..
            } = &join.relation
            {
                let join_alias = alias
                    .as_ref()
                    .map(|a| a.name.value.clone())
                    .unwrap_or_else(|| "subquery".to_string());
                let alias_columns = alias.as_ref().map(|a| a.columns.as_slice()).unwrap_or(&[]);

                let left_columns: Vec<String> = combined_schemas
                    .iter()
                    .flat_map(|(_, s)| s.columns.iter().map(|c| c.name.clone()))
                    .collect();

                let (join_condition, is_natural) = match &join.join_operator {
                    JoinOperator::Inner(JoinConstraint::On(expr)) => (Some(expr.clone()), false),
                    JoinOperator::LeftOuter(JoinConstraint::On(expr)) => (Some(expr.clone()), false),
                    JoinOperator::Inner(JoinConstraint::None) => (None, false),
                    JoinOperator::CrossJoin => (None, false),
                    JoinOperator::Inner(JoinConstraint::Using(cols))
                    | JoinOperator::LeftOuter(JoinConstraint::Using(cols)) => {
                        let using_cols: Vec<String> =
                            cols.iter().map(|c| normalize_ident(c)).collect();
                        if using_cols.is_empty() {
                            (None, true)
                        } else {
                            let left_alias = combined_schemas
                                .iter()
                                .find(|(_, s)| {
                                    s.columns
                                        .iter()
                                        .any(|c| c.name.eq_ignore_ascii_case(&using_cols[0]))
                                })
                                .map(|(a, _)| a.clone())
                                .unwrap_or_else(|| base_alias.clone());
                            let cond = using_cols
                                .iter()
                                .map(|col| Expr::BinaryOp {
                                    left: Box::new(Expr::CompoundIdentifier(vec![
                                        Ident::new(left_alias.clone()),
                                        Ident::new(col.clone()),
                                    ])),
                                    op: BinaryOperator::Eq,
                                    right: Box::new(Expr::CompoundIdentifier(vec![
                                        Ident::new(join_alias.clone()),
                                        Ident::new(col.clone()),
                                    ])),
                                })
                                .reduce(|a, b| Expr::BinaryOp {
                                    left: Box::new(a),
                                    op: BinaryOperator::And,
                                    right: Box::new(b),
                                })
                                .unwrap();
                            (Some(cond), true)
                        }
                    }
                    _ => return Err(anyhow!("Unsupported LATERAL JOIN type")),
                };
                let _ = is_natural;
                let _ = left_columns;

                let has_correlated_subquery = join_condition.as_ref().map_or(false, |cond| {
                    combined_schemas.iter().any(|(alias, _)| {
                        super::super::helpers::query_has_outer_reference_in_expr(cond, alias)
                    })
                });

                let join_condition = if let Some(cond) = join_condition {
                    if has_correlated_subquery {
                        Some(cond)
                    } else {
                        Some(
                            self.resolve_subqueries(txn, db_id, sequence_values, search_path, &cond, ctes)
                                .await?,
                        )
                    }
                } else {
                    None
                };

                let is_left_join = matches!(&join.join_operator, JoinOperator::LeftOuter(_));

                let mut join_schema: Option<TableSchema> = None;
                let mut join_column_offsets: Option<HashMap<String, usize>> = None;
                let mut join_combined_schema: Option<TableSchema> = None;

                let mut new_combined_rows = Vec::new();
                for left_row in &combined_rows {
                    let mut substituted_query = subquery.as_ref().clone();
                    let mut value_offset = 0;
                    for (alias, schema) in &combined_schemas {
                        let row_values: Vec<Value> = left_row.values
                            [value_offset..value_offset + schema.columns.len()]
                            .to_vec();
                        let outer_row = Row::new(row_values);
                        substituted_query = super::super::helpers::substitute_outer_values_in_query(
                            &substituted_query,
                            alias,
                            schema,
                            &outer_row,
                        );
                        value_offset += schema.columns.len();
                    }

                    let (this_schema, right_rows) = self
                        .execute_derived_table(
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            &substituted_query,
                            &join_alias,
                            alias_columns,
                            ctes,
                        )
                        .await?;

                    if join_schema.is_none() {
                        join_schema = Some(this_schema.clone());

                        let mut column_offsets: HashMap<String, usize> = HashMap::new();
                        let mut offset = 0;
                        for (alias, schema) in &combined_schemas {
                            for col in &schema.columns {
                                column_offsets.insert(format!("{}.{}", alias, col.name), offset);
                                if !column_offsets.contains_key(&col.name) {
                                    column_offsets.insert(col.name.clone(), offset);
                                }
                                offset += 1;
                            }
                        }
                        for col in &this_schema.columns {
                            column_offsets.insert(format!("{}.{}", join_alias, col.name), offset);
                            if !column_offsets.contains_key(&col.name) {
                                column_offsets.insert(col.name.clone(), offset);
                            }
                            if col.name.contains('.') {
                                column_offsets.insert(col.name.clone(), offset);
                            }
                            offset += 1;
                        }
                        join_column_offsets = Some(column_offsets);

                        let mut combined_col_defs: Vec<ColumnDef> = Vec::new();
                        for (_, schema) in &combined_schemas {
                            combined_col_defs.extend(schema.columns.clone());
                        }
                        combined_col_defs.extend(this_schema.columns.clone());
                        join_combined_schema = Some(TableSchema {
                            name: "joined".to_string(),
                            table_id: 0,
                            columns: combined_col_defs,
                            version: 1,
                            pk_constraint_name: None,
                            pk_indices: vec![],
                            indexes: vec![],
                            check_constraints: vec![],
                            foreign_keys: vec![],
                            owner: String::new(),
                        });
                    }

                    let Some(ref final_schema) = join_combined_schema else {
                        return Err(anyhow!("LATERAL join schema not initialized"));
                    };
                    let Some(ref final_column_offsets) = join_column_offsets else {
                        return Err(anyhow!("LATERAL join offsets not initialized"));
                    };

                    let mut matched = false;
                    for right_row in &right_rows {
                        let mut combined_values = left_row.values.clone();
                        combined_values.extend(right_row.values.clone());
                        let combined_row = Row::new(combined_values);

                        let ctx = JoinContext {
                            tables: HashMap::new(),
                            column_offsets: final_column_offsets.clone(),
                            combined_row: &combined_row,
                            combined_schema: final_schema,
                        };

                        let matches = if let Some(ref cond) = join_condition {
                            matches!(
                                self.eval_expr_join_maybe_sequence(
                                    txn,
                                    db_id,
                                    sequence_values,
                                    search_path,
                                    cond,
                                    &ctx
                                )
                                .await?,
                                Value::Boolean(true)
                            )
                        } else {
                            true
                        };

                        if matches {
                            new_combined_rows.push(combined_row);
                            matched = true;
                        }
                    }

                    if is_left_join && !matched {
                        let right_cols = join_schema
                            .as_ref()
                            .map(|s| s.columns.len())
                            .unwrap_or(0);
                        let mut combined_values = left_row.values.clone();
                        combined_values.extend(std::iter::repeat(Value::Null).take(right_cols));
                        new_combined_rows.push(Row::new(combined_values));
                    }
                }

                if join_schema.is_none() {
                    let (schema, _rows) = self
                        .execute_derived_table(
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            subquery,
                            &join_alias,
                            alias_columns,
                            ctes,
                        )
                        .await?;
                    join_schema = Some(schema);
                }

                combined_schemas.push((join_alias, join_schema.unwrap()));
                combined_rows = new_combined_rows;
                continue;
            }

            let (join_alias, join_schema, join_rows) = match &join.relation {
                TableFactor::Table {
                    name, alias, args, ..
                } => {
                    let (schema_opt, obj_name) = names::split_object_name(name)?;
                    let als = alias
                        .as_ref()
                        .map(|a| a.name.value.clone())
                        .unwrap_or_else(|| obj_name.clone());
                    let tbl_upper = obj_name.to_uppercase();

                    if tbl_upper == "GENERATE_SERIES" {
                        if let Some(func_args) = args {
                            let (schema, rows) = self
                                .execute_generate_series(func_args, &als, alias.as_ref())
                                .await?;
                            (als, schema, rows)
                        } else {
                            return Err(anyhow!("generate_series requires at least 2 arguments"));
                        }
                    } else {
                        let tbl = match schema_opt {
                            Some(schema) => format!("{}.{}", schema, obj_name),
                            None => obj_name.clone(),
                        };
                        let (schema, rows) = self
                            .get_table_data(txn, db_id, sequence_values, search_path, &tbl, ctes)
                            .await?;
                        (als, schema, rows)
                    }
                }
                TableFactor::Derived {
                    subquery, alias, ..
                } => {
                    let alias_name = alias
                        .as_ref()
                        .map(|a| a.name.value.clone())
                        .unwrap_or_else(|| "subquery".to_string());
                    let alias_columns = alias.as_ref().map(|a| a.columns.as_slice()).unwrap_or(&[]);
                    let (schema, rows) = self
                        .execute_derived_table(
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            subquery,
                            &alias_name,
                            alias_columns,
                            ctes,
                        )
                        .await?;
                    (alias_name, schema, rows)
                }
                TableFactor::NestedJoin { .. } => {
                    self.resolve_table_factor(
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        &join.relation,
                        ctes,
                    )
                    .await?
                }
                _ => return Err(anyhow!("Unsupported join table")),
            };

            let left_columns: Vec<String> = combined_schemas
                .iter()
                .flat_map(|(_, s)| s.columns.iter().map(|c| c.name.clone()))
                .collect();
            let right_columns: Vec<String> =
                join_schema.columns.iter().map(|c| c.name.clone()).collect();

            let (join_condition, is_natural) = match &join.join_operator {
                JoinOperator::Inner(JoinConstraint::On(expr)) => (Some(expr.clone()), false),
                JoinOperator::LeftOuter(JoinConstraint::On(expr)) => (Some(expr.clone()), false),
                JoinOperator::RightOuter(JoinConstraint::On(expr)) => (Some(expr.clone()), false),
                JoinOperator::FullOuter(JoinConstraint::On(expr)) => (Some(expr.clone()), false),
                JoinOperator::Inner(JoinConstraint::Natural)
                | JoinOperator::LeftOuter(JoinConstraint::Natural)
                | JoinOperator::RightOuter(JoinConstraint::Natural)
                | JoinOperator::FullOuter(JoinConstraint::Natural) => {
                    let common_cols: Vec<String> = left_columns
                        .iter()
                        .filter(|c| right_columns.contains(c))
                        .cloned()
                        .collect();
                    has_natural_join = true;
                    natural_join_common_cols = common_cols.clone();
                    if common_cols.is_empty() {
                        (None, true)
                    } else {
                        let cond = common_cols
                            .iter()
                            .map(|col| Expr::BinaryOp {
                                left: Box::new(Expr::CompoundIdentifier(vec![
                                    Ident::new(
                                        combined_schemas
                                            .iter()
                                            .find(|(_, s)| {
                                                s.columns
                                                    .iter()
                                                    .any(|c| c.name.eq_ignore_ascii_case(col))
                                            })
                                            .map(|(a, _)| a.clone())
                                            .unwrap_or_else(|| base_alias.clone()),
                                    ),
                                    Ident::new(col.clone()),
                                ])),
                                op: BinaryOperator::Eq,
                                right: Box::new(Expr::CompoundIdentifier(vec![
                                    Ident::new(join_alias.clone()),
                                    Ident::new(col.clone()),
                                ])),
                            })
                            .reduce(|a, b| Expr::BinaryOp {
                                left: Box::new(a),
                                op: BinaryOperator::And,
                                right: Box::new(b),
                            })
                            .unwrap();
                        (Some(cond), true)
                    }
                }
                JoinOperator::Inner(JoinConstraint::Using(cols))
                | JoinOperator::LeftOuter(JoinConstraint::Using(cols))
                | JoinOperator::RightOuter(JoinConstraint::Using(cols))
                | JoinOperator::FullOuter(JoinConstraint::Using(cols)) => {
                    let using_cols: Vec<String> = cols.iter().map(|c| normalize_ident(c)).collect();
                    has_natural_join = true;
                    natural_join_common_cols = using_cols.clone();
                    if using_cols.is_empty() {
                        (None, true)
                    } else {
                        let left_alias = combined_schemas
                            .iter()
                            .find(|(_, s)| {
                                s.columns
                                    .iter()
                                    .any(|c| c.name.eq_ignore_ascii_case(&using_cols[0]))
                            })
                            .map(|(a, _)| a.clone())
                            .unwrap_or_else(|| base_alias.clone());
                        let cond = using_cols
                            .iter()
                            .map(|col| Expr::BinaryOp {
                                left: Box::new(Expr::CompoundIdentifier(vec![
                                    Ident::new(left_alias.clone()),
                                    Ident::new(col.clone()),
                                ])),
                                op: BinaryOperator::Eq,
                                right: Box::new(Expr::CompoundIdentifier(vec![
                                    Ident::new(join_alias.clone()),
                                    Ident::new(col.clone()),
                                ])),
                            })
                            .reduce(|a, b| Expr::BinaryOp {
                                left: Box::new(a),
                                op: BinaryOperator::And,
                                right: Box::new(b),
                            })
                            .unwrap();
                        (Some(cond), true)
                    }
                }
                JoinOperator::CrossJoin => (None, false),
                JoinOperator::Inner(JoinConstraint::None) => (None, false),
                _ => return Err(anyhow!("Unsupported JOIN type")),
            };
            let _ = is_natural;

            let has_correlated_subquery = join_condition.as_ref().map_or(false, |cond| {
                combined_schemas.iter().any(|(alias, _)| {
                    super::super::helpers::query_has_outer_reference_in_expr(cond, alias)
                })
            });

            let join_condition = if let Some(cond) = join_condition {
                if has_correlated_subquery {
                    Some(cond)
                } else {
                    Some(
                        self.resolve_subqueries(txn, db_id, sequence_values, search_path, &cond, ctes)
                            .await?,
                    )
                }
            } else {
                None
            };

            let is_left_join = matches!(&join.join_operator, JoinOperator::LeftOuter(_));
            let is_right_join = matches!(&join.join_operator, JoinOperator::RightOuter(_));
            let is_full_join = matches!(&join.join_operator, JoinOperator::FullOuter(_));

            let left_col_count: usize = combined_schemas.iter().map(|(_, s)| s.columns.len()).sum();

            let mut column_offsets: HashMap<String, usize> = HashMap::new();
            let mut offset = 0;
            for (alias, schema) in &combined_schemas {
                for col in &schema.columns {
                    column_offsets.insert(format!("{}.{}", alias, col.name), offset);
                    if !column_offsets.contains_key(&col.name) {
                        column_offsets.insert(col.name.clone(), offset);
                    }
                    offset += 1;
                }
            }
            let _join_start_offset = offset;
            for col in &join_schema.columns {
                column_offsets.insert(format!("{}.{}", join_alias, col.name), offset);
                if !column_offsets.contains_key(&col.name) {
                    column_offsets.insert(col.name.clone(), offset);
                }
                if col.name.contains('.') {
                    column_offsets.insert(col.name.clone(), offset);
                }
                offset += 1;
            }

            let mut combined_col_defs: Vec<ColumnDef> = Vec::new();
            for (_, schema) in &combined_schemas {
                combined_col_defs.extend(schema.columns.clone());
            }
            combined_col_defs.extend(join_schema.columns.clone());
            let temp_combined_schema = TableSchema {
                name: "joined".to_string(),
                table_id: 0,
                columns: combined_col_defs,
                version: 1,
                pk_constraint_name: None,
                pk_indices: vec![],
                indexes: vec![],
                check_constraints: vec![],
                foreign_keys: vec![],
                owner: String::new(),
            };

            let hash_join_config = HashJoinConfig::default();
            let join_key_indices = if !has_correlated_subquery {
                join_condition.as_ref().and_then(|cond| {
                    extract_equi_join_key_indices_for_hash_join(
                        cond,
                        &column_offsets,
                        left_col_count,
                        join_schema.columns.len(),
                    )
                })
            } else {
                None
            }
            .and_then(|(lk, rk)| {
                if lk.is_empty() || lk.len() != rk.len() {
                    return None;
                }
                for (li, ri) in lk.iter().copied().zip(rk.iter().copied()) {
                    let left_dt = temp_combined_schema.columns.get(li).map(|c| &c.data_type)?;
                    let right_dt = temp_combined_schema
                        .columns
                        .get(left_col_count + ri)
                        .map(|c| &c.data_type)?;
                    if !hash_join_key_types_compatible(left_dt, right_dt) {
                        return None;
                    }
                }
                Some((lk, rk))
            });

            let use_hash_join = join_key_indices.is_some()
                && (combined_rows.len() + join_rows.len()) >= hash_join_config.min_rows_threshold;

            let left_outer = is_left_join || is_full_join;
            let right_outer = is_right_join || is_full_join;

            let mut new_combined_rows = Vec::new();
            if use_hash_join {
                let (left_key_indices, right_key_indices) = join_key_indices.unwrap();
                let mut right_matched: Vec<bool> = if right_outer {
                    vec![false; join_rows.len()]
                } else {
                    Vec::new()
                };

                // Choose the smaller side as build, but preserve the legacy output order
                // (left outer loop, right inner loop).
                let left_is_build = combined_rows.len() <= join_rows.len();

                if left_is_build {
                    // Build hash table on left, probe right. Record matches per left row to
                    // preserve output ordering.
                    let mut buckets: HashMap<u64, Vec<usize>> =
                        HashMap::with_capacity((combined_rows.len() as f64 * 1.4) as usize);
                    for (li, left_row) in combined_rows.iter().enumerate() {
                        if row_key_has_null_for_join(left_row, &left_key_indices) {
                            continue;
                        }
                        let hash = hash_row_key_for_join(left_row, &left_key_indices);
                        buckets.entry(hash).or_default().push(li);
                    }

                    let mut left_matches: Vec<Vec<usize>> = vec![Vec::new(); combined_rows.len()];
                    for (ri, right_row) in join_rows.iter().enumerate() {
                        if row_key_has_null_for_join(right_row, &right_key_indices) {
                            continue;
                        }
                        let hash = hash_row_key_for_join(right_row, &right_key_indices);
                        let Some(candidates) = buckets.get(&hash) else {
                            continue;
                        };
                        for &li in candidates {
                            let left_row = &combined_rows[li];
                            if !row_keys_equal_for_join(
                                left_row,
                                &left_key_indices,
                                right_row,
                                &right_key_indices,
                            ) {
                                continue;
                            }
                            left_matches[li].push(ri);
                            if right_outer {
                                right_matched[ri] = true;
                            }
                        }
                    }

                    for (li, left_row) in combined_rows.iter().enumerate() {
                        let matches = &left_matches[li];
                        if !matches.is_empty() {
                            for &ri in matches {
                                let right_row = &join_rows[ri];
                                let mut values = Vec::with_capacity(
                                    left_row.values.len() + right_row.values.len(),
                                );
                                values.extend(left_row.values.iter().cloned());
                                values.extend(right_row.values.iter().cloned());
                                new_combined_rows.push(Row::new(values));
                            }
                        } else if left_outer {
                            let mut values = Vec::with_capacity(
                                left_row.values.len() + join_schema.columns.len(),
                            );
                            values.extend(left_row.values.iter().cloned());
                            values.extend(
                                std::iter::repeat(Value::Null).take(join_schema.columns.len()),
                            );
                            new_combined_rows.push(Row::new(values));
                        }
                    }
                } else {
                    // Build hash table on right, probe left (legacy ordering).
                    let mut buckets: HashMap<u64, Vec<usize>> =
                        HashMap::with_capacity((join_rows.len() as f64 * 1.4) as usize);
                    for (ri, right_row) in join_rows.iter().enumerate() {
                        if row_key_has_null_for_join(right_row, &right_key_indices) {
                            continue;
                        }
                        let hash = hash_row_key_for_join(right_row, &right_key_indices);
                        buckets.entry(hash).or_default().push(ri);
                    }

                    for left_row in &combined_rows {
                        let mut matched = false;

                        if !row_key_has_null_for_join(left_row, &left_key_indices) {
                            let hash = hash_row_key_for_join(left_row, &left_key_indices);
                            if let Some(candidates) = buckets.get(&hash) {
                                for &ri in candidates {
                                    let right_row = &join_rows[ri];
                                    if !row_keys_equal_for_join(
                                        left_row,
                                        &left_key_indices,
                                        right_row,
                                        &right_key_indices,
                                    ) {
                                        continue;
                                    }
                                    let mut values = Vec::with_capacity(
                                        left_row.values.len() + right_row.values.len(),
                                    );
                                    values.extend(left_row.values.iter().cloned());
                                    values.extend(right_row.values.iter().cloned());
                                    new_combined_rows.push(Row::new(values));
                                    matched = true;
                                    if right_outer {
                                        right_matched[ri] = true;
                                    }
                                }
                            }
                        }

                        if left_outer && !matched {
                            let mut values = Vec::with_capacity(
                                left_row.values.len() + join_schema.columns.len(),
                            );
                            values.extend(left_row.values.iter().cloned());
                            values.extend(
                                std::iter::repeat(Value::Null).take(join_schema.columns.len()),
                            );
                            new_combined_rows.push(Row::new(values));
                        }
                    }
                }

                if right_outer {
                    for (ri, right_row) in join_rows.iter().enumerate() {
                        if !right_matched[ri] {
                            let mut values = Vec::with_capacity(left_col_count + right_row.values.len());
                            values.extend(std::iter::repeat(Value::Null).take(left_col_count));
                            values.extend(right_row.values.iter().cloned());
                            new_combined_rows.push(Row::new(values));
                        }
                    }
                }
            } else {
                let mut right_matched: Vec<bool> = vec![false; join_rows.len()];

                for left_row in &combined_rows {
                    let mut matched = false;

                    let resolved_condition = if has_correlated_subquery {
                        if let Some(ref cond) = join_condition {
                            let mut substituted = cond.clone();
                            let mut value_offset = 0;
                            for (alias, schema) in &combined_schemas {
                                let row_values: Vec<Value> = left_row.values
                                    [value_offset..value_offset + schema.columns.len()]
                                    .to_vec();
                                let outer_row = Row::new(row_values);
                                substituted = super::super::helpers::substitute_outer_values(
                                    &substituted,
                                    alias,
                                    schema,
                                    &outer_row,
                                );
                                value_offset += schema.columns.len();
                            }
                            Some(
                                self.resolve_subqueries(
                                    txn,
                                    db_id,
                                    sequence_values,
                                    search_path,
                                    &substituted,
                                    ctes,
                                )
                                .await?,
                            )
                        } else {
                            None
                        }
                    } else {
                        join_condition.clone()
                    };

                    for (right_idx, right_row) in join_rows.iter().enumerate() {
                        let mut combined_values = Vec::with_capacity(
                            left_row.values.len() + right_row.values.len(),
                        );
                        combined_values.extend(left_row.values.iter().cloned());
                        combined_values.extend(right_row.values.iter().cloned());
                        let combined_row = Row::new(combined_values);

                        let matches = if let Some(ref cond) = resolved_condition {
                            let ctx = JoinContext {
                                tables: HashMap::new(),
                                column_offsets: column_offsets.clone(),
                                combined_row: &combined_row,
                                combined_schema: &temp_combined_schema,
                            };
                            matches!(
                                self.eval_expr_join_maybe_sequence(
                                    txn,
                                    db_id,
                                    sequence_values,
                                    search_path,
                                    cond,
                                    &ctx
                                )
                                .await?,
                                Value::Boolean(true)
                            )
                        } else {
                            true
                        };

                        if matches {
                            new_combined_rows.push(combined_row);
                            matched = true;
                            right_matched[right_idx] = true;
                        }
                    }
                    if (is_left_join || is_full_join) && !matched {
                        let mut combined_values = Vec::with_capacity(
                            left_row.values.len() + join_schema.columns.len(),
                        );
                        combined_values.extend(left_row.values.iter().cloned());
                        combined_values.extend(
                            std::iter::repeat(Value::Null).take(join_schema.columns.len()),
                        );
                        new_combined_rows.push(Row::new(combined_values));
                    }
                }

                if is_right_join || is_full_join {
                    for (right_idx, right_row) in join_rows.iter().enumerate() {
                        if !right_matched[right_idx] {
                            let mut combined_values: Vec<Value> =
                                Vec::with_capacity(left_col_count + right_row.values.len());
                            combined_values
                                .extend(std::iter::repeat(Value::Null).take(left_col_count));
                            combined_values.extend(right_row.values.iter().cloned());
                            new_combined_rows.push(Row::new(combined_values));
                        }
                    }
                }
            }

            combined_schemas.push((join_alias.clone(), join_schema));
            combined_rows = new_combined_rows;
        }

        for (extra_schemas, extra_rows) in extra_from_items {
            let mut new_combined_rows =
                Vec::with_capacity(combined_rows.len().saturating_mul(extra_rows.len()));
            for left_row in &combined_rows {
                for right_row in &extra_rows {
                    let mut combined_values =
                        Vec::with_capacity(left_row.values.len() + right_row.values.len());
                    combined_values.extend(left_row.values.iter().cloned());
                    combined_values.extend(right_row.values.iter().cloned());
                    new_combined_rows.push(Row::new(combined_values));
                }
            }
            combined_schemas.extend(extra_schemas);
            combined_rows = new_combined_rows;
        }

        let mut final_column_offsets: HashMap<String, usize> = HashMap::new();
        let mut final_columns: Vec<ColumnDef> = Vec::new();
        let mut offset = 0;
        for (alias, schema) in &combined_schemas {
            for col in &schema.columns {
                final_column_offsets.insert(format!("{}.{}", alias, col.name), offset);
                if !final_column_offsets.contains_key(&col.name) {
                    final_column_offsets.insert(col.name.clone(), offset);
                }
                if col.name.contains('.') {
                    final_column_offsets.insert(col.name.clone(), offset);
                }
                final_columns.push(col.clone());
                offset += 1;
            }
        }
        let final_schema = TableSchema {
            name: "joined".to_string(),
            table_id: 0,
            columns: final_columns,
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
        };
        let type_infer_schema = build_type_infer_schema_for_join(&combined_schemas);

        // Resolve subqueries (EXISTS, IN (SELECT ...), scalar subqueries) in WHERE clause
        let resolved_selection = if let Some(sel) = &select.selection {
            Some(
                self.resolve_subqueries(txn, db_id, sequence_values, search_path, sel, ctes)
                    .await?,
            )
        } else {
            None
        };

        let filtered_rows = if let Some(ref sel) = resolved_selection {
            let mut v = Vec::new();
            for row in combined_rows {
                let ctx = JoinContext {
                    tables: HashMap::new(),
                    column_offsets: final_column_offsets.clone(),
                    combined_row: &row,
                    combined_schema: &final_schema,
                };
                if matches!(
                    self.eval_expr_join_maybe_sequence(
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        sel,
                        &ctx
                    )
                    .await?,
                    Value::Boolean(true)
                ) {
                    v.push(row);
                }
            }
            v
        } else {
            combined_rows
        };

        let outer_aliases: Vec<String> = combined_schemas.iter().map(|(a, _)| a.clone()).collect();
        let resolved_projection = self
            .resolve_projection_subqueries_for_join(
                txn,
                db_id,
                sequence_values,
                search_path,
                &select.projection,
                &outer_aliases,
                ctes,
            )
            .await?;

        let group_keys_exprs = match &select.group_by {
            GroupByExpr::Expressions(exprs) => exprs,
            GroupByExpr::All => return Err(anyhow!("GROUP BY ALL not supported")),
        };

        let mut agg_funcs: Vec<(usize, AggExpr)> = Vec::new();
        let extra_start = select.projection.len();
        for (i, item) in select.projection.iter().enumerate() {
            match item {
                SelectItem::UnnamedExpr(Expr::Function(f))
                | SelectItem::ExprWithAlias {
                    expr: Expr::Function(f),
                    ..
                } => {
                    if f.over.is_some() {
                        continue;
                    }
                    let func_name = f
                        .name
                        .0
                        .last()
                        .map(|i| i.value.to_uppercase())
                        .unwrap_or_default();
                    if matches!(
                        func_name.as_str(),
                        "COUNT" | "SUM" | "AVG" | "MAX" | "MIN" | "STRING_AGG" | "ARRAY_AGG"
                    ) {
                        agg_funcs.push((i, AggExpr::Function(f.clone())));
                    } else {
                        collect_having_agg_funcs(
                            &Expr::Function(f.clone()),
                            &mut agg_funcs,
                            extra_start,
                        );
                    }
                }
                SelectItem::UnnamedExpr(Expr::ArrayAgg(arr))
                | SelectItem::ExprWithAlias {
                    expr: Expr::ArrayAgg(arr),
                    ..
                } => {
                    agg_funcs.push((i, AggExpr::ArrayAgg(arr.clone())));
                }
                // Handle expressions containing nested aggregates (e.g., 'X=' || count(*))
                SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => {
                    collect_having_agg_funcs(expr, &mut agg_funcs, extra_start);
                }
                _ => {}
            }
        }

        if let Some(having_expr) = &select.having {
            collect_having_agg_funcs(having_expr, &mut agg_funcs, extra_start);
        }

        let is_agg = !group_keys_exprs.is_empty() || !agg_funcs.is_empty();

        if is_agg {
            let mut groups: HashMap<Vec<u8>, Vec<Aggregator>> = HashMap::new();
            let mut group_rows: HashMap<Vec<u8>, Row> = HashMap::new();
            // Track seen values for DISTINCT aggregates: group_key -> (agg_idx -> seen_values)
            let mut seen_distinct: HashMap<Vec<u8>, Vec<HashSet<Vec<u8>>>> = HashMap::new();

            for row in filtered_rows {
                let ctx = JoinContext {
                    tables: HashMap::new(),
                    column_offsets: final_column_offsets.clone(),
                    combined_row: &row,
                    combined_schema: &final_schema,
                };

                let mut key = Vec::new();
                for expr in group_keys_exprs {
                    key.push(
                        self.eval_expr_join_maybe_sequence(
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            expr,
                            &ctx,
                        )
                        .await?,
                    );
                }
                let key_bytes = bincode::serialize(&key).unwrap();

                if !groups.contains_key(&key_bytes) {
                    let mut aggs = Vec::new();
                    for (_, agg_expr) in &agg_funcs {
                        match agg_expr {
                            AggExpr::Function(f) => {
                                let name = f.name.0.last().unwrap().value.to_uppercase();
                                if name == "STRING_AGG" {
                                    let delimiter = if f.args.len() >= 2 {
                                        match &f.args[1] {
                                            FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => {
                                                match self
                                                    .eval_expr_join_maybe_sequence(
                                                        txn,
                                                        db_id,
                                                        sequence_values,
                                                        search_path,
                                                        e,
                                                        &ctx,
                                                    )
                                                    .await?
                                                {
                                                    Value::Text(s) => s,
                                                    _ => ",".to_string(),
                                                }
                                            }
                                            _ => ",".to_string(),
                                        }
                                    } else {
                                        ",".to_string()
                                    };
                                    aggs.push(Aggregator::new_string_agg(delimiter));
                                } else {
                                    aggs.push(Aggregator::new(&name)?);
                                }
                            }
                            AggExpr::ArrayAgg(_) => {
                                aggs.push(Aggregator::new_array_agg());
                            }
                        }
                    }
                    groups.insert(key_bytes.clone(), aggs);
                    group_rows.insert(key_bytes.clone(), row.clone());
                    let distinct_sets: Vec<HashSet<Vec<u8>>> =
                        agg_funcs.iter().map(|_| HashSet::new()).collect();
                    seen_distinct.insert(key_bytes.clone(), distinct_sets);
                }

                let aggs = groups.get_mut(&key_bytes).unwrap();
                for (agg_idx, (_, agg_expr)) in agg_funcs.iter().enumerate() {
                    let (filter_expr, arg_expr) = match agg_expr {
                        AggExpr::Function(f) => {
                            let filter = f.filter.as_ref().map(|e| e.as_ref());
                            let arg = if f.args.is_empty() {
                                None
                            } else {
                                match &f.args[0] {
                                    FunctionArg::Unnamed(FunctionArgExpr::Expr(e)) => Some(e),
                                    FunctionArg::Unnamed(FunctionArgExpr::Wildcard) => None,
                                    _ => return Err(anyhow!("Unsupported arg")),
                                }
                            };
                            (filter, arg)
                        }
                        AggExpr::ArrayAgg(arr) => (None, Some(arr.expr.as_ref())),
                    };

                    if let Some(filter) = filter_expr {
                        let filter_val = self
                            .eval_expr_join_maybe_sequence(
                                txn,
                                db_id,
                                sequence_values,
                                search_path,
                                filter,
                                &ctx,
                            )
                            .await?;
                        if !matches!(filter_val, Value::Boolean(true)) {
                            continue;
                        }
                    }

                    let val = if let Some(e) = arg_expr {
                        self.eval_expr_join_maybe_sequence(
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            e,
                            &ctx,
                        )
                        .await?
                    } else {
                        Value::Int32(1)
                    };

                    let is_distinct = matches!(agg_expr, AggExpr::Function(f) if f.distinct);
                    if is_distinct {
                        let val_bytes = bincode::serialize(&val).unwrap_or_default();
                        let distinct_sets = seen_distinct.get_mut(&key_bytes).unwrap();
                        if !distinct_sets[agg_idx].insert(val_bytes) {
                            continue;
                        }
                    }
                    aggs[agg_idx].update(&val)?;
                }
            }

            if groups.is_empty() && group_keys_exprs.is_empty() && !agg_funcs.is_empty() {
                // PostgreSQL semantics: aggregate query without GROUP BY returns exactly one row,
                // even when the input is empty (e.g., `SELECT COUNT(*) FROM empty` -> 0).
                let key: Vec<Value> = Vec::new();
                let key_bytes = bincode::serialize(&key).unwrap();

                let mut aggs = Vec::new();
                for (_, agg_expr) in &agg_funcs {
                    match agg_expr {
                        AggExpr::Function(f) => {
                            let name = f.name.0.last().unwrap().value.to_uppercase();
                            if name == "STRING_AGG" {
                                aggs.push(Aggregator::new_string_agg(",".to_string()));
                            } else {
                                aggs.push(Aggregator::new(&name)?);
                            }
                        }
                        AggExpr::ArrayAgg(_) => {
                            aggs.push(Aggregator::new_array_agg());
                        }
                    }
                }

                groups.insert(key_bytes.clone(), aggs);
                group_rows.insert(
                    key_bytes,
                    Row::new(vec![Value::Null; final_schema.columns.len()]),
                );
            }

            let mut final_rows = Vec::new();
            let mut final_rows_with_order_keys: Vec<(usize, Row, Vec<Value>)> = Vec::new();
            let col_names: Vec<String> =
                select.projection.iter().map(get_select_item_name).collect();

            let has_order_by = !query.order_by.is_empty();
            for (key_bytes, aggs) in groups {
                let representative = &group_rows[&key_bytes];
                let ctx = JoinContext {
                    tables: HashMap::new(),
                    column_offsets: final_column_offsets.clone(),
                    combined_row: representative,
                    combined_schema: &final_schema,
                };

                if let Some(having_expr) = &select.having {
                    let having_expr = if sequences::expr_needs_async_eval(having_expr) {
                        sequences::replace_sequence_functions_join(
                            &self.store(),
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            having_expr,
                            &ctx,
                        )
                        .await?
                    } else {
                        having_expr.clone()
                    };
                    let having_val = eval_having_expr_join(&having_expr, &ctx, &agg_funcs, &aggs)?;
                    if !matches!(having_val, Value::Boolean(true)) {
                        continue;
                    }
                }

                let mut row_values = Vec::new();
                for (i, item) in resolved_projection.iter().enumerate() {
                    if let Some(agg_pos) = agg_funcs.iter().position(|(idx, _)| *idx == i) {
                        row_values.push(aggs[agg_pos].result());
                    } else {
                        let expr = match item {
                            SelectItem::UnnamedExpr(e)
                            | SelectItem::ExprWithAlias { expr: e, .. } => e,
                            _ => return Err(anyhow!("Unsupported projection item")),
                        };
                        let expr = if sequences::expr_needs_async_eval(expr) {
                            sequences::replace_sequence_functions_join(
                                &self.store(),
                                txn,
                                db_id,
                                sequence_values,
                                search_path,
                                expr,
                                &ctx,
                            )
                            .await?
                        } else {
                            expr.clone()
                        };
                        row_values.push(eval_having_expr_join(&expr, &ctx, &agg_funcs, &aggs)?);
                    }
                }
                let row = Row::new(row_values);

                if has_order_by {
                    let mut keys = Vec::with_capacity(query.order_by.len());
                    for order_expr in &query.order_by {
                        match &order_expr.expr {
                            Expr::Value(sqlparser::ast::Value::Number(n, _)) => {
                                let idx = n.parse::<usize>().ok().map(|i| i.saturating_sub(1));
                                let key = idx
                                    .and_then(|i| row.values.get(i).cloned())
                                    .unwrap_or(Value::Null);
                                keys.push(key);
                            }
                            Expr::Identifier(ident) => {
                                if let Some(idx) = col_names
                                    .iter()
                                    .position(|n| n.eq_ignore_ascii_case(&ident.value))
                                {
                                    keys.push(row.values.get(idx).cloned().unwrap_or(Value::Null));
                                } else {
                                    let expr = if sequences::expr_needs_async_eval(&order_expr.expr) {
                                        sequences::replace_sequence_functions_join(
                                            &self.store(),
                                            txn,
                                            db_id,
                                            sequence_values,
                                            search_path,
                                            &order_expr.expr,
                                            &ctx,
                                        )
                                        .await?
                                    } else {
                                        order_expr.expr.clone()
                                    };
                                    keys.push(eval_having_expr_join(&expr, &ctx, &agg_funcs, &aggs)?);
                                }
                            }
                            _ => {
                                let expr = if sequences::expr_needs_async_eval(&order_expr.expr) {
                                    sequences::replace_sequence_functions_join(
                                        &self.store(),
                                        txn,
                                        db_id,
                                        sequence_values,
                                        search_path,
                                        &order_expr.expr,
                                        &ctx,
                                    )
                                    .await?
                                } else {
                                    order_expr.expr.clone()
                                };
                                keys.push(eval_having_expr_join(&expr, &ctx, &agg_funcs, &aggs)?);
                            }
                        }
                    }
                    final_rows_with_order_keys.push((final_rows_with_order_keys.len(), row, keys));
                } else {
                    final_rows.push(row);
                }
            }

            let final_rows = if has_order_by {
                final_rows_with_order_keys.sort_by(|(a_idx, _, a_keys), (b_idx, _, b_keys)| {
                    for (i, order_expr) in query.order_by.iter().enumerate() {
                        let val_a = a_keys.get(i).cloned().unwrap_or(Value::Null);
                        let val_b = b_keys.get(i).cloned().unwrap_or(Value::Null);
                        let asc = order_expr.asc.unwrap_or(true);
                        let nulls_first = order_expr.nulls_first.unwrap_or(!asc);

                        match (&val_a, &val_b) {
                            (Value::Null, Value::Null) => continue,
                            (Value::Null, _) => {
                                return if nulls_first {
                                    std::cmp::Ordering::Less
                                } else {
                                    std::cmp::Ordering::Greater
                                }
                            }
                            (_, Value::Null) => {
                                return if nulls_first {
                                    std::cmp::Ordering::Greater
                                } else {
                                    std::cmp::Ordering::Less
                                }
                            }
                            _ => {}
                        }

                        let cmp = super::super::expr::compare_values(&val_a, &val_b).unwrap_or(0);
                        if cmp != 0 {
                            return if asc {
                                if cmp > 0 {
                                    std::cmp::Ordering::Greater
                                } else {
                                    std::cmp::Ordering::Less
                                }
                            } else if cmp > 0 {
                                std::cmp::Ordering::Less
                            } else {
                                std::cmp::Ordering::Greater
                            };
                        }
                    }
                    a_idx.cmp(b_idx)
                });
                final_rows_with_order_keys
                    .into_iter()
                    .map(|(_, r, _)| r)
                    .collect()
            } else {
                final_rows
            };

            let final_rows = apply_offset_limit_fetch(final_rows, query);

            return Ok(ExecuteResult::Select {
                column_types: Some(
                    resolved_projection
                        .iter()
                        .map(|item| match item {
                            SelectItem::UnnamedExpr(expr)
                            | SelectItem::ExprWithAlias { expr, .. } => {
                                infer_expr_type(expr, &type_infer_schema)
                            }
                            _ => DataType::Text,
                        })
                        .collect(),
                ),
                columns: col_names,
                rows: final_rows,
            });
        }

        let window_funcs = extract_window_functions(&select.projection);
        let window_results = if !window_funcs.is_empty() {
            Some(compute_window_functions_join(
                &filtered_rows,
                &final_column_offsets,
                &final_schema,
                &window_funcs,
            )?)
        } else {
            None
        };

        let (filtered_rows, window_results) = if !query.order_by.is_empty() {
            let resolved_order_exprs: Vec<Expr> = query
                .order_by
                .iter()
                .map(|order_expr| {
                    if let Expr::Identifier(ref ident) = order_expr.expr {
                        for item in &resolved_projection {
                            if let SelectItem::ExprWithAlias { expr, alias } = item {
                                if alias.value.eq_ignore_ascii_case(&ident.value) {
                                    return expr.clone();
                                }
                            }
                        }
                    }
                    order_expr.expr.clone()
                })
                .collect();

            let order_by_uses_sequences = resolved_order_exprs
                .iter()
                .any(|e| sequences::expr_needs_async_eval(e));

            if order_by_uses_sequences {
                let mut rows_with_keys: Vec<(usize, Row, Vec<Value>)> =
                    Vec::with_capacity(filtered_rows.len());

                for (orig_idx, row) in filtered_rows.into_iter().enumerate() {
                    let ctx = JoinContext {
                        tables: HashMap::new(),
                        column_offsets: final_column_offsets.clone(),
                        combined_row: &row,
                        combined_schema: &final_schema,
                    };

                    let mut keys = Vec::with_capacity(resolved_order_exprs.len());
                    for expr in &resolved_order_exprs {
                        let val = if sequences::expr_needs_async_eval(expr) {
                            self.eval_expr_join_maybe_sequence(
                                txn,
                                db_id,
                                sequence_values,
                                search_path,
                                expr,
                                &ctx,
                            )
                            .await?
                        } else {
                            eval_expr_join(expr, &ctx).unwrap_or(Value::Null)
                        };
                        keys.push(val);
                    }
                    rows_with_keys.push((orig_idx, row, keys));
                }

                rows_with_keys.sort_by(|(_, _, a_keys), (_, _, b_keys)| {
                    for (idx, order_expr) in query.order_by.iter().enumerate() {
                        let val_a = a_keys.get(idx).cloned().unwrap_or(Value::Null);
                        let val_b = b_keys.get(idx).cloned().unwrap_or(Value::Null);
                        let cmp = super::super::expr::compare_values(&val_a, &val_b).unwrap_or(0);
                        if cmp != 0 {
                            let asc = order_expr.asc.unwrap_or(true);
                            return if asc {
                                if cmp > 0 {
                                    std::cmp::Ordering::Greater
                                } else {
                                    std::cmp::Ordering::Less
                                }
                            } else if cmp > 0 {
                                std::cmp::Ordering::Less
                            } else {
                                std::cmp::Ordering::Greater
                            };
                        }
                    }
                    std::cmp::Ordering::Equal
                });

                let reordered_wr = window_results.map(|wr| {
                    rows_with_keys
                        .iter()
                        .map(|(orig_idx, _, _)| wr[*orig_idx].clone())
                        .collect()
                });
                let reordered_rows: Vec<Row> =
                    rows_with_keys.into_iter().map(|(_, r, _)| r).collect();
                (reordered_rows, reordered_wr)
            } else {
                let mut indexed: Vec<(usize, Row)> =
                    filtered_rows.into_iter().enumerate().collect();
                indexed.sort_by(|(_, a), (_, b)| {
                    for (idx, order_expr) in query.order_by.iter().enumerate() {
                        let expr = &resolved_order_exprs[idx];
                        let ctx_a = JoinContext {
                            tables: HashMap::new(),
                            column_offsets: final_column_offsets.clone(),
                            combined_row: a,
                            combined_schema: &final_schema,
                        };
                        let ctx_b = JoinContext {
                            tables: HashMap::new(),
                            column_offsets: final_column_offsets.clone(),
                            combined_row: b,
                            combined_schema: &final_schema,
                        };
                        let val_a = eval_expr_join(expr, &ctx_a).unwrap_or(Value::Null);
                        let val_b = eval_expr_join(expr, &ctx_b).unwrap_or(Value::Null);
                        let cmp = super::super::expr::compare_values(&val_a, &val_b).unwrap_or(0);
                        if cmp != 0 {
                            let asc = order_expr.asc.unwrap_or(true);
                            return if asc {
                                if cmp > 0 {
                                    std::cmp::Ordering::Greater
                                } else {
                                    std::cmp::Ordering::Less
                                }
                            } else if cmp > 0 {
                                std::cmp::Ordering::Less
                            } else {
                                std::cmp::Ordering::Greater
                            };
                        }
                    }
                    std::cmp::Ordering::Equal
                });
                let reordered_wr = window_results.map(|wr| {
                    indexed
                        .iter()
                        .map(|(orig_idx, _)| wr[*orig_idx].clone())
                        .collect()
                });
                let reordered_rows: Vec<Row> = indexed.into_iter().map(|(_, r)| r).collect();
                (reordered_rows, reordered_wr)
            }
        } else {
            (filtered_rows, window_results)
        };

        let (rows_to_project, window_results) = match &select.distinct {
            Some(Distinct::On(on_exprs)) => {
                let (rows, indices) = distinct_on_rows_join_with_indices(
                    filtered_rows,
                    on_exprs,
                    &final_column_offsets,
                    &final_schema,
                )?;
                let window_results =
                    window_results.map(|wr| super::super::query::reorder_by_indices(&wr, &indices));
                (rows, window_results)
            }
            _ => (filtered_rows, window_results),
        };

        let mut cols = Vec::new();
        let mut result_rows = Vec::new();
        let mut column_types: Vec<DataType> = Vec::new();

        let has_wildcard = select
            .projection
            .iter()
            .any(|p| matches!(p, SelectItem::Wildcard(_)));
        let has_qualified_wildcard = select
            .projection
            .iter()
            .any(|p| matches!(p, SelectItem::QualifiedWildcard(..)));
        if has_wildcard {
            if has_natural_join && !natural_join_common_cols.is_empty() {
                (cols, column_types, result_rows) = project_wildcard_natural_join(
                    &combined_schemas,
                    &natural_join_common_cols,
                    rows_to_project,
                );
            } else {
                for (alias, schema) in &combined_schemas {
                    for col in &schema.columns {
                        cols.push(format!("{}.{}", alias, col.name));
                        column_types.push(col.data_type.clone());
                    }
                }
                result_rows = rows_to_project;
            }
        } else if has_qualified_wildcard {
            let mut expanded_items: Vec<(String, Option<(String, usize)>)> = Vec::new();

            let mut schema_offsets: HashMap<String, usize> = HashMap::new();
            let mut offset = 0;
            for (alias, schema) in &combined_schemas {
                schema_offsets.insert(alias.clone(), offset);
                offset += schema.columns.len();
            }

            for item in &select.projection {
                match item {
                    SelectItem::QualifiedWildcard(prefix, _) => {
                        let table_alias =
                            prefix.0.last().map(|i| i.value.clone()).unwrap_or_default();
                        if let Some((alias, schema)) = combined_schemas
                            .iter()
                            .find(|(a, _)| a.eq_ignore_ascii_case(&table_alias))
                        {
                            if let Some(&start_offset) = schema_offsets.get(alias) {
                                for (col_idx, col) in schema.columns.iter().enumerate() {
                                    expanded_items.push((
                                        col.name.clone(),
                                        Some((alias.clone(), start_offset + col_idx)),
                                    ));
                                }
                            }
                        }
                    }
                    SelectItem::UnnamedExpr(Expr::Identifier(id)) => {
                        expanded_items.push((id.value.clone(), None));
                    }
                    SelectItem::UnnamedExpr(Expr::CompoundIdentifier(parts)) => {
                        expanded_items.push((
                            parts
                                .last()
                                .map(|p| p.value.clone())
                                .unwrap_or_else(|| "col".to_string()),
                            None,
                        ));
                    }
                    SelectItem::ExprWithAlias { alias, .. } => {
                        expanded_items.push((alias.value.clone(), None));
                    }
                    SelectItem::UnnamedExpr(Expr::Function(f)) => {
                        expanded_items.push((
                            f.name
                                .0
                                .last()
                                .map(|i| i.value.clone())
                                .unwrap_or("func".to_string()),
                            None,
                        ));
                    }
                    _ => {
                        expanded_items.push(("col".to_string(), None));
                    }
                }
            }

            for (name, _) in &expanded_items {
                cols.push(name.clone());
            }

            for item in resolved_projection.iter() {
                match item {
                    SelectItem::QualifiedWildcard(prefix, _) => {
                        let table_alias =
                            prefix.0.last().map(|i| i.value.clone()).unwrap_or_default();
                        if let Some((_, schema)) = combined_schemas
                            .iter()
                            .find(|(a, _)| a.eq_ignore_ascii_case(&table_alias))
                        {
                            column_types
                                .extend(schema.columns.iter().map(|col| col.data_type.clone()));
                        }
                    }
                    SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => {
                        column_types.push(infer_expr_type(expr, &type_infer_schema));
                    }
                    _ => column_types.push(DataType::Text),
                }
            }

            let has_window_funcs = !window_funcs.is_empty();
            for (row_idx, row) in rows_to_project.iter().enumerate() {
                let ctx = JoinContext {
                    tables: HashMap::new(),
                    column_offsets: final_column_offsets.clone(),
                    combined_row: row,
                    combined_schema: &final_schema,
                };
                let mut vals = Vec::new();
                let mut proj_idx = 0;

                for item in resolved_projection.iter() {
                    match item {
                        SelectItem::QualifiedWildcard(prefix, _) => {
                            let table_alias =
                                prefix.0.last().map(|i| i.value.clone()).unwrap_or_default();
                            if let Some((alias, schema)) = combined_schemas
                                .iter()
                                .find(|(a, _)| a.eq_ignore_ascii_case(&table_alias))
                            {
                                if let Some(&start_offset) = schema_offsets.get(alias) {
                                    for col_idx in 0..schema.columns.len() {
                                        let value = row
                                            .values
                                            .get(start_offset + col_idx)
                                            .cloned()
                                            .unwrap_or(Value::Null);
                                        vals.push(value);
                                    }
                                }
                            }
                        }
                        SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => {
                            if has_window_funcs {
                                if let Some(wf_idx) =
                                    window_funcs.iter().position(|wf| wf.proj_idx == proj_idx)
                                {
                                    if let Some(ref wr) = window_results {
                                        vals.push(wr[row_idx][wf_idx].clone());
                                        proj_idx += 1;
                                        continue;
                                    }
                                }
                            }
                            let value = if let Expr::Subquery(subquery) = e {
                                self.eval_scalar_subquery_in_join(
                                    txn,
                                    db_id,
                                    sequence_values,
                                    search_path,
                                    subquery,
                                    &ctx,
                                    ctes,
                                )
                                .await?
                            } else {
                                self.eval_expr_join_maybe_sequence(
                                    txn,
                                    db_id,
                                    sequence_values,
                                    search_path,
                                    e,
                                    &ctx,
                                )
                                .await?
                            };
                            vals.push(value);
                        }
                        SelectItem::Wildcard(_) => {}
                    }
                    proj_idx += 1;
                }
                result_rows.push(Row::new(vals));
            }
        } else {
            for item in &select.projection {
                match item {
                    SelectItem::UnnamedExpr(Expr::Identifier(id)) => cols.push(id.value.clone()),
                    SelectItem::UnnamedExpr(Expr::CompoundIdentifier(parts)) => {
                        cols.push(
                            parts
                                .last()
                                .map(|p| p.value.clone())
                                .unwrap_or_else(|| "col".to_string()),
                        );
                    }
                    SelectItem::ExprWithAlias { alias, .. } => cols.push(alias.value.clone()),
                    SelectItem::UnnamedExpr(Expr::Function(f)) => {
                        cols.push(
                            f.name
                                .0
                                .last()
                                .map(|i| i.value.clone())
                                .unwrap_or("func".to_string()),
                        );
                    }
                    _ => cols.push("col".to_string()),
                }
            }

            column_types = resolved_projection
                .iter()
                .map(|item| match item {
                    SelectItem::UnnamedExpr(expr) | SelectItem::ExprWithAlias { expr, .. } => {
                        infer_expr_type(expr, &type_infer_schema)
                    }
                    _ => DataType::Text,
                })
                .collect();

            let has_window_funcs = !window_funcs.is_empty();
            for (row_idx, row) in rows_to_project.iter().enumerate() {
                let ctx = JoinContext {
                    tables: HashMap::new(),
                    column_offsets: final_column_offsets.clone(),
                    combined_row: row,
                    combined_schema: &final_schema,
                };
                let mut vals = Vec::new();
                for (proj_idx, item) in resolved_projection.iter().enumerate() {
                    if has_window_funcs {
                        if let Some(wf_idx) =
                            window_funcs.iter().position(|wf| wf.proj_idx == proj_idx)
                        {
                            if let Some(ref wr) = window_results {
                                vals.push(wr[row_idx][wf_idx].clone());
                                continue;
                            }
                        }
                    }
                    let expr = match item {
                        SelectItem::UnnamedExpr(e) | SelectItem::ExprWithAlias { expr: e, .. } => e,
                        SelectItem::Wildcard(_) => continue,
                        _ => return Err(anyhow!("Unsupported select item")),
                    };
                    let value = if let Expr::Subquery(subquery) = expr {
                        self.eval_scalar_subquery_in_join(
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            subquery,
                            &ctx,
                            ctes,
                        )
                        .await?
                    } else {
                        self.eval_expr_join_maybe_sequence(
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            expr,
                            &ctx,
                        )
                        .await?
                    };
                    vals.push(value);
                }
                result_rows.push(Row::new(vals));
            }
        }

        if matches!(&select.distinct, Some(Distinct::Distinct)) {
            result_rows = dedup_rows(result_rows);
        }

        result_rows = apply_offset_limit_fetch(result_rows, query);

        Ok(ExecuteResult::Select {
            column_types: Some(column_types),
            columns: cols,
            rows: result_rows,
        })
    }
}

fn generate_series_values(
    start: &Value,
    stop: &Value,
    step: &Value,
) -> Result<(Vec<Value>, DataType)> {
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
            let mut values = Vec::new();
            if step_val > 0 {
                let mut current = *s;
                while current <= *e {
                    values.push(Value::Int32(current));
                    current = match current.checked_add(step_val) {
                        Some(next) => next,
                        None => break,
                    };
                }
            } else {
                let mut current = *s;
                while current >= *e {
                    values.push(Value::Int32(current));
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
            let mut values = Vec::new();
            if step_val > 0 {
                let mut current = *s;
                while current <= *e {
                    values.push(Value::Int64(current));
                    current = match current.checked_add(step_val) {
                        Some(next) => next,
                        None => break,
                    };
                }
            } else {
                let mut current = *s;
                while current >= *e {
                    values.push(Value::Int64(current));
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
            let mut values = Vec::new();
            if step_val > 0 {
                let mut current = s64;
                while current <= e64 {
                    values.push(Value::Int64(current));
                    current = match current.checked_add(step_val) {
                        Some(next) => next,
                        None => break,
                    };
                }
            } else {
                let mut current = s64;
                while current >= e64 {
                    values.push(Value::Int64(current));
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
            let mut values = Vec::new();
            if step_val > 0.0 {
                let mut current = *s;
                while current <= *e + f64::EPSILON {
                    values.push(Value::Float64(current));
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
            } else {
                let mut current = *s;
                while current >= *e - f64::EPSILON {
                    values.push(Value::Float64(current));
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
            }
            Ok((values, DataType::Float64))
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
            let mut values = Vec::new();
            if step_ms > 0 {
                let mut current = *s;
                while current <= *e {
                    values.push(Value::Timestamp(current));
                    current = match current.checked_add(step_ms) {
                        Some(next) => next,
                        None => break,
                    };
                }
            } else {
                let mut current = *s;
                while current >= *e {
                    values.push(Value::Timestamp(current));
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
            let step_days = interval_to_days(&step_interval);
            if step_days == 0 {
                return Err(anyhow!("step size cannot equal zero"));
            }
            let mut values = Vec::new();
            let tz = chrono_tz::America::Los_Angeles;
            if step_days > 0 {
                let mut current = *s;
                while current <= *e {
                    let date = crate::types::date::date_days_to_naive_date(current)?;
                    let naive = date
                        .and_hms_opt(0, 0, 0)
                        .ok_or_else(|| anyhow!("Invalid date"))?;
                    let local = tz
                        .from_local_datetime(&naive)
                        .single()
                        .ok_or_else(|| anyhow!("Invalid local timestamptz"))?;
                    values.push(Value::Timestamp(local.timestamp_millis()));
                    current = match current.checked_add(step_days) {
                        Some(next) => next,
                        None => break,
                    };
                }
            } else {
                let mut current = *s;
                while current >= *e {
                    let date = crate::types::date::date_days_to_naive_date(current)?;
                    let naive = date
                        .and_hms_opt(0, 0, 0)
                        .ok_or_else(|| anyhow!("Invalid date"))?;
                    let local = tz
                        .from_local_datetime(&naive)
                        .single()
                        .ok_or_else(|| anyhow!("Invalid local timestamptz"))?;
                    values.push(Value::Timestamp(local.timestamp_millis()));
                    current = match current.checked_add(step_days) {
                        Some(next) => next,
                        None => break,
                    };
                }
            }
            Ok((values, DataType::TimestampTz))
        }
        (Value::Numeric(s), Value::Numeric(e)) => {
            let step_val = match step {
                Value::Null => rust_decimal::Decimal::ONE,
                Value::Numeric(st) => *st,
                Value::Float64(st) => {
                    rust_decimal::Decimal::try_from(*st).map_err(|_| {
                        anyhow!("invalid input syntax for type numeric: \"{}\"", st)
                    })?
                }
                Value::Int32(st) => rust_decimal::Decimal::from(*st),
                Value::Int64(st) => rust_decimal::Decimal::from(*st),
                _ => return Err(anyhow!("Invalid step type for numeric generate_series")),
            };
            if step_val.is_zero() {
                return Err(anyhow!("step size cannot equal zero"));
            }
            let mut values = Vec::new();
            if step_val > rust_decimal::Decimal::ZERO {
                let mut current = *s;
                while current <= *e {
                    values.push(Value::Numeric(current));
                    if current == *e {
                        break;
                    }
                    current = match current.checked_add(step_val) {
                        Some(next) => next,
                        None => break,
                    };
                }
            } else {
                let mut current = *s;
                while current >= *e {
                    values.push(Value::Numeric(current));
                    if current == *e {
                        break;
                    }
                    current = match current.checked_add(step_val) {
                        Some(next) => next,
                        None => break,
                    };
                }
            }
            Ok((
                values,
                DataType::Numeric {
                    precision: None,
                    scale: None,
                },
            ))
        }
        _ => Err(anyhow!(
            "generate_series requires numeric or timestamp arguments, got {:?} and {:?}",
            start,
            stop
        )),
    }
}

fn interval_to_millis(iv: &crate::types::IntervalValue) -> i64 {
    iv.to_millis_approx()
}

fn interval_to_days(iv: &crate::types::IntervalValue) -> i32 {
    let months_days = iv.months * 30;
    let millis_days = (iv.millis / (1000 * 60 * 60 * 24)) as i32;
    months_days + millis_days
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn natural_join_wildcard_projection_keeps_common_cols_first_and_in_order() {
        let schema_a = TableSchema {
            name: "a".to_string(),
            table_id: 1,
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
                    name: "id".to_string(),
                    data_type: DataType::Int32,
                    nullable: false,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                },
                ColumnDef {
                    name: "a1".to_string(),
                    data_type: DataType::Text,
                    nullable: false,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                },
            ],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
        };

        let schema_b = TableSchema {
            name: "b".to_string(),
            table_id: 2,
            columns: vec![
                ColumnDef {
                    name: "id".to_string(),
                    data_type: DataType::Int32,
                    nullable: false,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                },
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
                    name: "b1".to_string(),
                    data_type: DataType::Text,
                    nullable: false,
                    primary_key: false,
                    unique: false,
                    is_serial: false,
                    default_expr: None,
                },
            ],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
        };

        let combined_schemas = vec![("a".to_string(), schema_a), ("b".to_string(), schema_b)];
        let using_cols = vec!["id".to_string(), "name".to_string()];

        let rows = vec![Row::new(vec![
            Value::Text("alice".to_string()), // a.name
            Value::Int32(1),                  // a.id
            Value::Text("a1".to_string()),    // a.a1
            Value::Int32(1),                  // b.id
            Value::Text("alice".to_string()), // b.name
            Value::Text("b1".to_string()),    // b.b1
        ])];

        let (cols, types, projected) =
            project_wildcard_natural_join(&combined_schemas, &using_cols, rows);

        assert_eq!(cols, vec!["id", "name", "a1", "b1"]);
        assert_eq!(
            types,
            vec![DataType::Int32, DataType::Text, DataType::Text, DataType::Text]
        );
        assert_eq!(
            projected[0].values,
            vec![
                Value::Int32(1),
                Value::Text("alice".to_string()),
                Value::Text("a1".to_string()),
                Value::Text("b1".to_string()),
            ]
        );
    }

    #[test]
    fn join_type_inference_schema_resolves_qualified_columns() {
        let col_a_id = ColumnDef {
            name: "id".to_string(),
            data_type: DataType::Int32,
            nullable: false,
            primary_key: false,
            unique: false,
            is_serial: false,
            default_expr: None,
        };
        let col_b_id = ColumnDef {
            name: "id".to_string(),
            data_type: DataType::Text,
            nullable: false,
            primary_key: false,
            unique: false,
            is_serial: false,
            default_expr: None,
        };

        let schema_a = TableSchema {
            name: "a".to_string(),
            table_id: 1,
            columns: vec![col_a_id],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
        };
        let schema_b = TableSchema {
            name: "b".to_string(),
            table_id: 2,
            columns: vec![col_b_id],
            version: 1,
            pk_constraint_name: None,
            pk_indices: vec![],
            indexes: vec![],
            check_constraints: vec![],
            foreign_keys: vec![],
            owner: String::new(),
        };

        let combined_schemas = vec![("a".to_string(), schema_a), ("b".to_string(), schema_b)];
        let schema = build_type_infer_schema_for_join(&combined_schemas);

        let a_id = Expr::CompoundIdentifier(vec![Ident::new("a"), Ident::new("id")]);
        let b_id = Expr::CompoundIdentifier(vec![Ident::new("b"), Ident::new("id")]);
        let schema_a_id = Expr::CompoundIdentifier(vec![
            Ident::new("public"),
            Ident::new("a"),
            Ident::new("id"),
        ]);
        let unqualified_id = Expr::Identifier(Ident::new("id"));

        assert_eq!(infer_expr_type(&a_id, &schema), DataType::Int32);
        assert_eq!(infer_expr_type(&b_id, &schema), DataType::Text);
        assert_eq!(
            infer_expr_type(&schema_a_id, &schema),
            DataType::Int32,
            "schema-qualified identifiers should resolve using the table alias",
        );
        assert_eq!(
            infer_expr_type(&unqualified_id, &schema),
            DataType::Int32,
            "Unqualified columns should resolve to the left-most match",
        );
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
        let step = Value::Interval(crate::types::IntervalValue {
            months: i32::MAX / 30,
            millis: 7 * 24 * 60 * 60 * 1000,
        });
        let (values, ty) = generate_series_values(&Value::Date(1), &Value::Date(1), &step).unwrap();
        assert_eq!(ty, DataType::TimestampTz);

        let tz = chrono_tz::America::Los_Angeles;
        let date = crate::types::date::date_days_to_naive_date(1).unwrap();
        let naive = date.and_hms_opt(0, 0, 0).unwrap();
        let local = tz.from_local_datetime(&naive).single().unwrap();
        assert_eq!(values, vec![Value::Timestamp(local.timestamp_millis())]);
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
