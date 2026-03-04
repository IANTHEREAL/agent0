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
use crate::sql::error::SqlError;
use crate::sql::expr::static_eval::needs_async_materialization;
use crate::sql::expr::typed_eval::eval_typed_expr;
use crate::sql::expr::typed_fold::fold_typed_expr;
use crate::sql::query_context::QueryContext;
use crate::sql::sequences::SequenceSession;
use crate::sql::session::DEFAULT_DML_TABLE_SCAN_MAX_ROWS;
use anyhow::{anyhow, Result};
use std::collections::HashMap;
use tikv_client::Transaction;

// ── Executor helper methods ─────────────────────────────────

impl Executor {
    /// Resolve an AnalyzedTableRef to a table name + schema + rows.
    ///
    /// Supports Table, Subquery (derived tables, VALUES), and Join variants.
    /// Uses Box::pin for the Join recursive case.
    #[allow(clippy::type_complexity)]
    pub(super) fn resolve_and_scan_table_ref<'a>(
        &'a self,
        txn: &'a mut Transaction,
        db_id: u64,
        search_path: &'a [String],
        table_ref: &'a crate::sql::analyzer::types::AnalyzedTableRef,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = Result<(String, TableSchema, Vec<Row>)>> + Send + 'a>,
    > {
        Box::pin(async move {
            use crate::sql::analyzer::types::AnalyzedTableRefKind;
            match &table_ref.kind {
                AnalyzedTableRefKind::Table { name, .. } => {
                    let schema = self
                        .store()
                        .get_schema(txn, db_id, name)
                        .await?
                        .ok_or_else(|| anyhow!("Table '{}' does not exist", name))?;
                    let max_rows = dml_table_scan_max_rows_from_settings();
                    let row_cap = dml_table_scan_row_cap(max_rows);
                    let mut rows = self
                        .scan_and_fill_with_limit(txn, db_id, name, &schema, row_cap)
                        .await?;
                    if row_cap_reached(rows.len(), row_cap) {
                        return Err(SqlError::DmlTableScanTooLarge {
                            message: format!(
                                "table \"{}\" contains more than {} rows, exceeding \
                                 the UPDATE FROM / DELETE USING auxiliary table limit \
                                 (set db9.dml_table_scan_max_rows to adjust or 0 to \
                                 disable)",
                                name, max_rows
                            ),
                        }
                        .into());
                    }
                    append_ctid_to_rows(&mut rows);
                    Ok((name.clone(), schema, rows))
                }
                AnalyzedTableRefKind::Subquery(query) => {
                    let alias = table_ref
                        .alias
                        .clone()
                        .unwrap_or_else(|| "__subquery".to_string());
                    let mut seq_vals = SequenceSession::new();
                    let empty_ctes: HashMap<String, (TableSchema, Vec<Row>)> = HashMap::new();
                    let max_rows = dml_table_scan_max_rows_from_settings();
                    let row_cap = dml_table_scan_row_cap(max_rows);
                    let cap_scope = row_cap.unwrap_or(0);
                    let result = crate::session_context::with_dml_limit_cap(cap_scope, {
                        let txn = &mut *txn;
                        self.execute_subquery(
                            txn,
                            db_id,
                            &mut seq_vals,
                            search_path,
                            query,
                            &empty_ctes,
                        )
                    })
                    .await?;
                    match result {
                        crate::sql::ExecuteResult::Select { rows, .. } => {
                            if row_cap_reached(rows.len(), row_cap) {
                                return Err(SqlError::DmlTableScanTooLarge {
                                    message: format!(
                                        "subquery \"{}\" returned more than {} rows, \
                                         exceeding the UPDATE FROM / DELETE USING \
                                         auxiliary table limit (set \
                                         db9.dml_table_scan_max_rows to adjust or 0 \
                                         to disable)",
                                        alias, max_rows
                                    ),
                                }
                                .into());
                            }
                            use crate::sql::executor::select::analyzed::postprocess::build_output_schema;
                            let schema = build_output_schema(query);
                            Ok((alias, schema, rows))
                        }
                        _ => Err(anyhow!("Expected SELECT result from subquery in FROM")),
                    }
                }
                AnalyzedTableRefKind::Join {
                    left,
                    right,
                    join_type,
                    condition,
                    left_col_start,
                } => {
                    let alias = table_ref
                        .alias
                        .clone()
                        .unwrap_or_else(|| "__join".to_string());
                    let (_l_name, l_schema, l_rows) = self
                        .resolve_and_scan_table_ref(txn, db_id, search_path, left)
                        .await?;
                    let (_r_name, r_schema, r_rows) = self
                        .resolve_and_scan_table_ref(txn, db_id, search_path, right)
                        .await?;

                    let qctx = QueryContext::from_task_locals();
                    let max_rows = dml_table_scan_max_rows_from_settings();
                    let mut joined_rows = Vec::new();

                    use crate::sql::analyzer::types::JoinType;
                    match join_type {
                        JoinType::Inner | JoinType::Cross => {
                            for lr in &l_rows {
                                for rr in &r_rows {
                                    let combined = combine_rows(lr, rr);
                                    if self.join_condition_matches(
                                        condition,
                                        &combined,
                                        *left_col_start,
                                        &qctx,
                                    )? {
                                        joined_rows.push(combined);
                                        check_join_output_limit(joined_rows.len(), max_rows)?;
                                    }
                                }
                            }
                        }
                        JoinType::Left => {
                            let ctid_slots = count_ctid_slots(right);
                            let r_cols = r_rows
                                .first()
                                .map_or(r_schema.columns.len() + ctid_slots, |r| r.values.len());
                            for lr in &l_rows {
                                let mut found = false;
                                for rr in &r_rows {
                                    let combined = combine_rows(lr, rr);
                                    if self.join_condition_matches(
                                        condition,
                                        &combined,
                                        *left_col_start,
                                        &qctx,
                                    )? {
                                        joined_rows.push(combined);
                                        check_join_output_limit(joined_rows.len(), max_rows)?;
                                        found = true;
                                    }
                                }
                                if !found {
                                    let null_right = Row::new(vec![Value::Null; r_cols]);
                                    joined_rows.push(combine_rows(lr, &null_right));
                                    check_join_output_limit(joined_rows.len(), max_rows)?;
                                }
                            }
                        }
                        JoinType::Right => {
                            // RIGHT JOIN = swap sides and do LEFT JOIN logic.
                            // For each right row, find matching left rows;
                            // emit NULL-padded left when no match.
                            // Use runtime row width (includes synthetic ctid from table scans)
                            // rather than bare schema width which omits system columns.
                            // Only base table inputs get ctid; subquery/derived do not.
                            let l_ctid = count_ctid_slots(left);
                            let l_cols = l_rows
                                .first()
                                .map_or(l_schema.columns.len() + l_ctid, |r| r.values.len());
                            for rr in &r_rows {
                                let mut found = false;
                                for lr in &l_rows {
                                    let combined = combine_rows(lr, rr);
                                    if self.join_condition_matches(
                                        condition,
                                        &combined,
                                        *left_col_start,
                                        &qctx,
                                    )? {
                                        joined_rows.push(combined);
                                        check_join_output_limit(joined_rows.len(), max_rows)?;
                                        found = true;
                                    }
                                }
                                if !found {
                                    let null_left = Row::new(vec![Value::Null; l_cols]);
                                    joined_rows.push(combine_rows(&null_left, rr));
                                    check_join_output_limit(joined_rows.len(), max_rows)?;
                                }
                            }
                        }
                        JoinType::Full => {
                            // FULL OUTER JOIN: LEFT JOIN + unmatched right rows.
                            // Use runtime row width (includes synthetic ctid from table scans)
                            // rather than bare schema width which omits system columns.
                            // Only base table inputs get ctid; subquery/derived do not.
                            let l_ctid = count_ctid_slots(left);
                            let r_ctid = count_ctid_slots(right);
                            let l_cols = l_rows
                                .first()
                                .map_or(l_schema.columns.len() + l_ctid, |r| r.values.len());
                            let r_cols = r_rows
                                .first()
                                .map_or(r_schema.columns.len() + r_ctid, |r| r.values.len());

                            // Track which right rows matched at least once.
                            let mut right_matched = vec![false; r_rows.len()];

                            // Left-join pass: for each left row, find matches
                            // on the right; emit NULL-padded right when none.
                            for lr in &l_rows {
                                let mut found = false;
                                for (ri, rr) in r_rows.iter().enumerate() {
                                    let combined = combine_rows(lr, rr);
                                    if self.join_condition_matches(
                                        condition,
                                        &combined,
                                        *left_col_start,
                                        &qctx,
                                    )? {
                                        joined_rows.push(combined);
                                        check_join_output_limit(joined_rows.len(), max_rows)?;
                                        found = true;
                                        right_matched[ri] = true;
                                    }
                                }
                                if !found {
                                    let null_right = Row::new(vec![Value::Null; r_cols]);
                                    joined_rows.push(combine_rows(lr, &null_right));
                                    check_join_output_limit(joined_rows.len(), max_rows)?;
                                }
                            }

                            // Anti-join pass: emit unmatched right rows with
                            // NULL-padded left side.
                            for (ri, rr) in r_rows.iter().enumerate() {
                                if !right_matched[ri] {
                                    let null_left = Row::new(vec![Value::Null; l_cols]);
                                    joined_rows.push(combine_rows(&null_left, rr));
                                    check_join_output_limit(joined_rows.len(), max_rows)?;
                                }
                            }
                        }
                    }

                    // Build combined schema
                    let mut combined_cols: Vec<crate::model::ColumnDef> = Vec::new();
                    for col in &l_schema.columns {
                        combined_cols.push(col.clone());
                    }
                    for col in &r_schema.columns {
                        combined_cols.push(col.clone());
                    }
                    let combined_schema = TableSchema::new(alias.clone(), 0, combined_cols, vec![]);

                    Ok((alias, combined_schema, joined_rows))
                }
                _ => Err(anyhow!("unsupported table reference in DML USING/FROM")),
            }
        })
    }

    /// Check if a join condition matches for a combined row.
    ///
    /// `left_col_start` is the global scope offset where this join's left
    /// child columns begin.  The analyzer resolves ON-condition column
    /// references against the full scope (which may include preceding tables
    /// such as the UPDATE target), but the `combined` row only contains
    /// columns from the join's own left + right sides.  We prepend
    /// `left_col_start` NULL placeholders so column indices line up.
    fn join_condition_matches(
        &self,
        condition: &crate::sql::analyzer::types::JoinCondition,
        combined: &Row,
        left_col_start: usize,
        qctx: &QueryContext,
    ) -> Result<bool> {
        use crate::sql::analyzer::types::JoinCondition;
        match condition {
            JoinCondition::On(expr) => {
                let eval_row = if left_col_start > 0 {
                    let mut padded = Vec::with_capacity(left_col_start + combined.values.len());
                    padded.resize(left_col_start, Value::Null);
                    padded.extend_from_slice(&combined.values);
                    Row::new(padded)
                } else {
                    combined.clone()
                };
                let val = eval_typed_expr(expr, &eval_row, qctx)?;
                typed_value_to_bool(val)
            }
            JoinCondition::None => Ok(true),
            JoinCondition::Using(cols) => {
                for col in cols {
                    let lv = combined.values.get(col.left_index).unwrap_or(&Value::Null);
                    let rv = combined.values.get(col.right_index).unwrap_or(&Value::Null);
                    if lv != rv {
                        return Ok(false);
                    }
                }
                Ok(true)
            }
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
        sequence_values: &mut SequenceSession,
        search_path: &[String],
        schema: &TableSchema,
        row_vals: &mut [Value],
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
        sequence_values: &mut SequenceSession,
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
    ///
    /// Subquery execution is wrapped with `with_dml_limit_cap` so that any
    /// `LIMIT` inside a DML expression subquery (e.g. `UPDATE SET col =
    /// (SELECT ... LIMIT $1)`, `DELETE WHERE col IN (SELECT ... LIMIT $1)`)
    /// is clamped by `db9.dml_table_scan_max_rows`.
    pub(super) async fn eval_typed_expr_maybe_async(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut SequenceSession,
        search_path: &[String],
        expr: &TypedExpr,
        eval_row: &Row,
        schema: Option<&TableSchema>,
        ctes: &HashMap<String, (TableSchema, Vec<Row>)>,
        qctx: &QueryContext,
    ) -> Result<Value> {
        if needs_async_materialization(expr) {
            let row_cap = dml_table_scan_row_cap_from_settings().unwrap_or(0);
            let materialized = crate::session_context::with_dml_limit_cap(
                row_cap,
                self.materialize_expr_for_row(
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
                ),
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
                    .scan_queue_entries_for_task(
                        &mut txn,
                        &keyspace,
                        db_id,
                        table_id as i64,
                        crate::worker::types::TaskType::AutoAnalyze,
                    )
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

/// Returns the number of synthetic ctid slots contributed by a table ref.
/// Base tables contribute 1 each; subqueries and functions contribute 0;
/// joins recurse into both sides.
fn count_ctid_slots(table_ref: &crate::sql::analyzer::types::AnalyzedTableRef) -> usize {
    use crate::sql::analyzer::types::AnalyzedTableRefKind;
    match &table_ref.kind {
        AnalyzedTableRefKind::Table { .. } => 1,
        AnalyzedTableRefKind::Subquery(_) | AnalyzedTableRefKind::Function { .. } => 0,
        AnalyzedTableRefKind::Join { left, right, .. } => {
            count_ctid_slots(left) + count_ctid_slots(right)
        }
    }
}

/// Read the `db9.dml_table_scan_max_rows` setting from the current session's
/// settings snapshot. Returns default when no snapshot is available.
pub(super) fn dml_table_scan_max_rows_from_settings() -> usize {
    let qctx = QueryContext::from_task_locals();
    qctx.settings_snapshot
        .as_ref()
        .and_then(|s| s.get("db9.dml_table_scan_max_rows"))
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_DML_TABLE_SCAN_MAX_ROWS)
}

/// Convert configured `max_rows` to internal `row_cap`.
///
/// `row_cap = max_rows + 1` enables deterministic overflow detection:
/// collecting `row_cap` rows means "returned more than max_rows rows".
fn dml_table_scan_row_cap(max_rows: usize) -> Option<usize> {
    if max_rows == 0 {
        None
    } else {
        Some(max_rows.saturating_add(1))
    }
}

pub(super) fn dml_table_scan_row_cap_from_settings() -> Option<usize> {
    dml_table_scan_row_cap(dml_table_scan_max_rows_from_settings())
}

fn row_cap_reached(len: usize, row_cap: Option<usize>) -> bool {
    row_cap.is_some_and(|cap| len >= cap)
}

/// Streaming post-join row guard: checks whether the join output has exceeded
/// the configured `max_rows` limit. Called after each row is pushed to the
/// output during nested-loop join execution.
fn check_join_output_limit(joined_rows_len: usize, max_rows: usize) -> Result<()> {
    if max_rows > 0 && joined_rows_len > max_rows {
        return Err(SqlError::DmlTableScanTooLarge {
            message: format!(
                "JOIN produced more than {} rows, exceeding the UPDATE FROM / \
                 DELETE USING auxiliary table limit (set \
                 db9.dml_table_scan_max_rows to adjust or 0 to disable)",
                max_rows
            ),
        }
        .into());
    }
    Ok(())
}

/// Check that the combined cross-product of auxiliary row counts does not
/// exceed the configured statement limit.
///
/// This catches cases where each source is under per-source limit but their
/// product would still explode at materialization time.
pub(super) fn check_cross_product_limit(sizes: &[usize], max_rows: usize) -> Result<()> {
    if max_rows == 0 || sizes.is_empty() {
        return Ok(());
    }
    let product: usize = sizes
        .iter()
        .try_fold(1usize, |acc, &n| acc.checked_mul(n))
        .unwrap_or(usize::MAX);
    if product > max_rows {
        let size_strs: Vec<String> = sizes.iter().map(|n| n.to_string()).collect();
        return Err(SqlError::DmlTableScanTooLarge {
            message: format!(
                "combined auxiliary table row count ({}) exceeds the UPDATE FROM / \
                 DELETE USING limit of {} rows (individual table sizes: {}; \
                 set db9.dml_table_scan_max_rows to adjust or 0 to disable)",
                product,
                max_rows,
                size_strs.join(" × "),
            ),
        }
        .into());
    }
    Ok(())
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

/// Build cross-product of rows from multiple tables with optional max-row guard.
///
/// Given [[A1, A2], [B1, B2, B3]], produces:
/// [A1+B1, A1+B2, A1+B3, A2+B1, A2+B2, A2+B3]
/// where + means value concatenation.
pub(super) fn cross_product_rows(table_rows: &[Vec<Row>], max_rows: usize) -> Result<Vec<Row>> {
    if table_rows.is_empty() {
        return Ok(vec![]);
    }
    let sizes: Vec<usize> = table_rows.iter().map(|rows| rows.len()).collect();
    check_cross_product_limit(&sizes, max_rows)?;
    let row_cap = dml_table_scan_row_cap(max_rows);
    let mut result = table_rows[0].clone();
    for table in &table_rows[1..] {
        let mut new_result = Vec::with_capacity(result.len() * table.len());
        for left in &result {
            for right in table {
                new_result.push(combine_rows(left, right));
                if row_cap_reached(new_result.len(), row_cap) {
                    return Err(SqlError::DmlTableScanTooLarge {
                        message: format!(
                            "combined auxiliary table row count exceeds the UPDATE FROM / \
                             DELETE USING limit of {} rows during cartesian \
                             materialization (set db9.dml_table_scan_max_rows \
                             to adjust or 0 to disable)",
                            max_rows
                        ),
                    }
                    .into());
                }
            }
        }
        result = new_result;
    }
    Ok(result)
}

/// Check if a TypedExpr is a DEFAULT placeholder.
pub(super) fn is_default_typed_expr(expr: &TypedExpr) -> bool {
    matches!(expr.kind, TypedExprKind::Default)
}

/// Append a monotonic ctid ordinal (0-based Int64) to each row.
///
/// The ctid value is appended as the last element, matching the synthetic
/// ctid column position registered in the analyzer scope by `add_table`.
pub(super) fn append_ctid_to_rows(rows: &mut [Row]) {
    for (i, row) in rows.iter_mut().enumerate() {
        row.values.push(Value::Int64(i as i64));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::DataType;
    use crate::sql::analyzer::types::TypedExprKind;

    fn col_ref(idx: usize, name: &str, dt: DataType) -> TypedExpr {
        TypedExpr::new(
            TypedExprKind::ColumnRef {
                scope_depth: 0,
                column_index: idx,
                column_name: name.to_string(),
            },
            dt,
        )
    }

    #[test]
    fn combine_rows_concatenates_values() {
        let left = Row::new(vec![Value::Int32(1), Value::Text("a".to_string())]);
        let right = Row::new(vec![Value::Boolean(true)]);
        let combined = combine_rows(&left, &right);
        assert_eq!(
            combined.values,
            vec![
                Value::Int32(1),
                Value::Text("a".to_string()),
                Value::Boolean(true)
            ]
        );
    }

    #[test]
    fn cross_product_rows_builds_cartesian_product() {
        let a1 = Row::new(vec![Value::Text("a1".to_string())]);
        let a2 = Row::new(vec![Value::Text("a2".to_string())]);
        let b1 = Row::new(vec![Value::Text("b1".to_string())]);
        let b2 = Row::new(vec![Value::Text("b2".to_string())]);
        let rows = cross_product_rows(&[vec![a1, a2], vec![b1, b2]], 0).unwrap();
        let got: Vec<Vec<Value>> = rows.into_iter().map(|r| r.values).collect();
        assert_eq!(
            got,
            vec![
                vec![Value::Text("a1".to_string()), Value::Text("b1".to_string())],
                vec![Value::Text("a1".to_string()), Value::Text("b2".to_string())],
                vec![Value::Text("a2".to_string()), Value::Text("b1".to_string())],
                vec![Value::Text("a2".to_string()), Value::Text("b2".to_string())],
            ]
        );
    }

    #[test]
    fn cross_product_rows_handles_empty_input() {
        assert!(cross_product_rows(&[], 0).unwrap().is_empty());
    }

    #[test]
    fn check_cross_product_limit_rejects_oversized_product() {
        let err = check_cross_product_limit(&[4, 4], 5).expect_err("must reject 16 > 5");
        let msg = err.to_string();
        assert!(msg.contains("combined auxiliary table row count (16)"));
        assert!(msg.contains("limit of 5 rows"));
    }

    #[test]
    fn check_cross_product_limit_accepts_disabled_or_small() {
        check_cross_product_limit(&[4, 4], 0).expect("0 disables guard");
        check_cross_product_limit(&[2, 2], 5).expect("4 <= 5");
    }

    #[test]
    fn typed_value_to_bool_handles_bool_and_null() {
        assert!(typed_value_to_bool(Value::Boolean(true)).unwrap());
        assert!(!typed_value_to_bool(Value::Boolean(false)).unwrap());
        assert!(!typed_value_to_bool(Value::Null).unwrap());
    }

    #[test]
    fn typed_value_to_bool_rejects_non_boolean() {
        let err = typed_value_to_bool(Value::Int32(1)).unwrap_err();
        assert!(err.to_string().contains("WHERE must be boolean"));
    }

    #[test]
    fn returning_schema_helpers_use_analyzed_projection_metadata() {
        let projections = vec![
            AnalyzedProjection {
                output_name: "id".to_string(),
                expr: col_ref(0, "id", DataType::Int32),
            },
            AnalyzedProjection {
                output_name: "name".to_string(),
                expr: col_ref(1, "name", DataType::Text),
            },
        ];
        let schema = TableSchema::new("t".to_string(), 1, vec![], vec![]);

        let cols = build_returning_columns_from_analyzed(&Some(projections.clone()), &schema);
        let tys = build_returning_types_from_analyzed(&Some(projections), &schema);
        assert_eq!(cols, vec!["id".to_string(), "name".to_string()]);
        assert_eq!(tys, vec![DataType::Int32, DataType::Text]);
    }

    #[test]
    fn eval_returning_typed_evaluates_projection_expressions() {
        let returning = vec![
            AnalyzedProjection {
                output_name: "id".to_string(),
                expr: col_ref(0, "id", DataType::Int32),
            },
            AnalyzedProjection {
                output_name: "const_name".to_string(),
                expr: TypedExpr::new(
                    TypedExprKind::Constant(Value::Text("alice".to_string())),
                    DataType::Text,
                ),
            },
        ];
        let row = Row::new(vec![Value::Int32(7), Value::Text("ignored".to_string())]);
        let out = eval_returning_typed(&returning, &row, &QueryContext::from_task_locals())
            .expect("returning eval should succeed");
        assert_eq!(
            out.values,
            vec![Value::Int32(7), Value::Text("alice".to_string())]
        );
    }

    #[test]
    fn count_ctid_slots_returns_correct_counts() {
        use crate::sql::analyzer::types::{
            AnalyzedTableRef, AnalyzedTableRefKind, JoinCondition, JoinType, TableRefSchema,
        };

        let make_table = |name: &str| AnalyzedTableRef {
            kind: AnalyzedTableRefKind::Table {
                name: name.to_string(),
                schema: TableRefSchema {
                    table_id: 0,
                    columns: vec![],
                },
            },
            alias: None,
        };

        // Base table → 1
        assert_eq!(count_ctid_slots(&make_table("t")), 1);

        // Function → 0
        let func_ref = AnalyzedTableRef {
            kind: AnalyzedTableRefKind::Function {
                func: crate::sql::analyzer::types::ResolvedFunction {
                    name: "unnest".to_string(),
                    kind: crate::sql::analyzer::types::FunctionKind::Builtin,
                    return_type: DataType::Int32,
                },
                args: vec![],
                output_columns: vec![],
            },
            alias: None,
        };
        assert_eq!(count_ctid_slots(&func_ref), 0);

        // Join of two tables → 2
        let join2 = AnalyzedTableRef {
            kind: AnalyzedTableRefKind::Join {
                left: Box::new(make_table("a")),
                right: Box::new(make_table("b")),
                join_type: JoinType::Inner,
                condition: JoinCondition::None,
                left_col_start: 0,
            },
            alias: None,
        };
        assert_eq!(count_ctid_slots(&join2), 2);

        // Nested: (a JOIN b) JOIN c → 3
        let join3 = AnalyzedTableRef {
            kind: AnalyzedTableRefKind::Join {
                left: Box::new(join2),
                right: Box::new(make_table("c")),
                join_type: JoinType::Inner,
                condition: JoinCondition::None,
                left_col_start: 0,
            },
            alias: None,
        };
        assert_eq!(count_ctid_slots(&join3), 3);
    }

    #[test]
    fn is_default_typed_expr_detects_default_variant() {
        assert!(is_default_typed_expr(&TypedExpr::new(
            TypedExprKind::Default,
            DataType::Text
        )));
        assert!(!is_default_typed_expr(&TypedExpr::new(
            TypedExprKind::Constant(Value::Null),
            DataType::Text
        )));
    }
}
