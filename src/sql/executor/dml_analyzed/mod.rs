//! Analyzed DML execution: INSERT, UPDATE, DELETE from typed IR.
//!
//! These execute DML from `AnalyzedInsert`, `AnalyzedUpdate`, `AnalyzedDelete`
//! (produced by the Analyzer). All expressions are pre-typed `TypedExpr` --
//! evaluated via `eval_typed_expr`, no bridge/NullCatalog needed.

mod delete;
mod insert;
mod update;

use super::super::dml;
use super::core::Executor;
use crate::model::{Row, TableSchema, Value};
use crate::sql::analyzer::types::{AnalyzedProjection, TypedExpr, TypedExprKind};
use crate::sql::expr::static_eval::needs_async_materialization;
use crate::sql::expr::typed_eval::eval_typed_expr;
use crate::sql::expr::typed_fold::fold_typed_expr;
use crate::sql::query_context::QueryContext;
use anyhow::{anyhow, Result};
use std::collections::HashMap;
use tikv_client::Transaction;

// ── Executor helper methods ─────────────────────────────────

impl Executor {
    /// Resolve an AnalyzedTableRef to a table name + schema + rows.
    pub(super) async fn resolve_and_scan_table_ref(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        _search_path: &[String],
        table_ref: &crate::sql::analyzer::types::AnalyzedTableRef,
    ) -> Result<(String, TableSchema, Vec<Row>)> {
        use crate::sql::analyzer::types::AnalyzedTableRefKind;
        match &table_ref.kind {
            AnalyzedTableRefKind::Table { name, .. } => {
                let schema = self
                    .store()
                    .get_schema(txn, db_id, name)
                    .await?
                    .ok_or_else(|| anyhow!("Table '{}' does not exist", name))?;
                let rows = self.scan_and_fill(txn, db_id, name, &schema).await?;
                Ok((name.clone(), schema, rows))
            }
            _ => Err(anyhow!("unsupported table reference in DML USING/FROM")),
        }
    }

    /// Fill in default values for columns not in the INSERT target list.
    ///
    /// Delegates to the existing `dml::fill_missing_columns` which handles
    /// serial columns (nextval) and DEFAULT expressions.
    pub(super) async fn fill_defaults_for_row(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        schema: &TableSchema,
        row_vals: &mut Vec<Value>,
        target_columns: &[usize],
    ) -> Result<()> {
        dml::fill_missing_columns(
            &self.store(),
            txn,
            db_id,
            sequence_values,
            search_path,
            schema,
            row_vals,
            target_columns,
        )
        .await
    }

    /// Evaluate a typed DML assignment value for UPDATE / ON CONFLICT DO UPDATE.
    pub(super) async fn eval_assignment_value(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        schema: &TableSchema,
        col_idx: usize,
        typed_expr: &TypedExpr,
        eval_row: &Row,
        qctx: &QueryContext,
    ) -> Result<Value> {
        let folded_expr = fold_typed_expr(typed_expr, qctx);
        if is_default_typed_expr(&folded_expr) {
            return dml::eval_column_default_or_null(
                &self.store(),
                txn,
                db_id,
                sequence_values,
                search_path,
                schema,
                col_idx,
            )
            .await;
        }

        let empty_ctes: HashMap<String, (TableSchema, Vec<Row>)> = HashMap::new();
        self.eval_typed_expr_maybe_async(
            txn,
            db_id,
            sequence_values,
            search_path,
            &folded_expr,
            eval_row,
            Some(schema),
            &empty_ctes,
            qctx,
        )
        .await
    }

    /// Evaluate typed expression in DML path, materializing async constructs
    /// (subqueries, sequence/cat funcs) on the current row when needed.
    pub(super) async fn eval_typed_expr_maybe_async(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        expr: &TypedExpr,
        eval_row: &Row,
        schema: Option<&TableSchema>,
        ctes: &HashMap<String, (TableSchema, Vec<Row>)>,
        qctx: &QueryContext,
    ) -> Result<Value> {
        if needs_async_materialization(expr) {
            let materialized = self
                .materialize_expr_for_row(
                    expr,
                    eval_row,
                    None,
                    schema,
                    txn,
                    db_id,
                    sequence_values,
                    search_path,
                    ctes,
                    qctx,
                )
                .await?;
            eval_typed_expr(&materialized, eval_row, qctx)
        } else {
            eval_typed_expr(expr, eval_row, qctx)
        }
    }

    /// Best-effort, non-blocking enqueue of auto-ANALYZE when modification
    /// count exceeds the PostgreSQL-style threshold (`base + 0.1 * row_est`).
    pub(super) fn maybe_enqueue_auto_analyze(&self, db_id: u64, table_id: u64, table_name: &str) {
        const AUTO_ANALYZE_THRESHOLD: u64 = 50;
        if !self
            .stats_cache()
            .needs_auto_analyze(db_id, table_id, AUTO_ANALYZE_THRESHOLD)
        {
            return;
        }
        // Reset immediately to prevent repeated enqueues from concurrent DML.
        self.stats_cache().reset_mod_count(db_id, table_id);

        let keyspace = self.tenant_keyspace().to_string();
        let table_name = table_name.to_string();
        tokio::spawn(async move {
            let Some(system_store) = crate::worker::get_system_store() else {
                return;
            };
            let store = system_store.clone();
            let entry = crate::worker::types::TaskQueueEntry::new(
                keyspace.clone(),
                db_id,
                table_id as i64,
                crate::worker::types::TaskType::AutoAnalyze,
                format!("ANALYZE \"{}\"", table_name),
                "system".to_string(),
                128,
            );
            let result: Result<bool, anyhow::Error> = async {
                let mut txn = store.begin().await?;
                let existing = store
                    .scan_queue_entries_for_task(&mut txn, &keyspace, db_id, table_id as i64)
                    .await?;
                if existing.is_empty() {
                    let now_ms = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis() as i64;
                    store
                        .put_worker_queue_entry(&mut txn, &entry, now_ms)
                        .await?;
                    store
                        .update_registry_task_types(
                            &mut txn,
                            &keyspace,
                            db_id,
                            crate::worker::types::TASK_TYPE_AUTO_ANALYZE,
                            0,
                        )
                        .await?;
                    txn.commit().await?;
                    Ok(true)
                } else {
                    txn.rollback().await.ok();
                    Ok(false)
                }
            }
            .await;
            match result {
                Ok(true) => crate::worker::wake_worker(),
                Ok(false) => {}
                Err(e) => {
                    tracing::warn!("Failed to enqueue auto-ANALYZE for {}: {}", table_name, e);
                }
            }
        });
    }
}

// ── Free helpers ────────────────────────────────────────────

/// Combine two rows (left + right) for join-style evaluation.
pub(super) fn combine_rows(left: &Row, right: &Row) -> Row {
    let mut values = left.values.clone();
    values.extend(right.values.clone());
    Row::new(values)
}

/// Convert a typed expression result to bool (with NULL -> false).
pub(super) fn typed_value_to_bool(val: Value) -> Result<bool> {
    match val {
        Value::Boolean(b) => Ok(b),
        Value::Null => Ok(false),
        other => Err(anyhow!("WHERE must be boolean, got {:?}", other)),
    }
}

/// Evaluate a RETURNING clause from analyzed projections.
pub(super) fn eval_returning_typed(
    returning: &[AnalyzedProjection],
    row: &Row,
    qctx: &QueryContext,
) -> Result<Row> {
    let mut values = Vec::with_capacity(returning.len());
    for proj in returning {
        let val = eval_typed_expr(&proj.expr, row, qctx)?;
        values.push(val);
    }
    Ok(Row::new(values))
}

/// Build RETURNING column names from analyzed projections.
pub(super) fn build_returning_columns_from_analyzed(
    returning: &Option<Vec<AnalyzedProjection>>,
    _schema: &TableSchema,
) -> Vec<String> {
    match returning {
        Some(projs) => projs.iter().map(|p| p.output_name.clone()).collect(),
        None => vec![],
    }
}

/// Build RETURNING column types from analyzed projections.
pub(super) fn build_returning_types_from_analyzed(
    returning: &Option<Vec<AnalyzedProjection>>,
    _schema: &TableSchema,
) -> Vec<crate::model::DataType> {
    match returning {
        Some(projs) => projs.iter().map(|p| p.expr.data_type.clone()).collect(),
        None => vec![],
    }
}

/// Build cross-product of rows from multiple tables.
///
/// Given [[A1, A2], [B1, B2, B3]], produces:
/// [A1+B1, A1+B2, A1+B3, A2+B1, A2+B2, A2+B3]
/// where + means value concatenation.
pub(super) fn cross_product_rows(table_rows: &[Vec<Row>]) -> Vec<Row> {
    if table_rows.is_empty() {
        return vec![];
    }
    let mut result = table_rows[0].clone();
    for table in &table_rows[1..] {
        let mut new_result = Vec::with_capacity(result.len() * table.len());
        for left in &result {
            for right in table {
                new_result.push(combine_rows(left, right));
            }
        }
        result = new_result;
    }
    result
}

/// Check if a TypedExpr is a DEFAULT placeholder.
pub(super) fn is_default_typed_expr(expr: &TypedExpr) -> bool {
    matches!(expr.kind, TypedExprKind::Default)
}
