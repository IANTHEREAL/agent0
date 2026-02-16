//! Analyzed DML execution: INSERT, UPDATE, DELETE from typed IR.
//!
//! These execute DML from `AnalyzedInsert`, `AnalyzedUpdate`, `AnalyzedDelete`
//! (produced by the Analyzer). All expressions are pre-typed `TypedExpr` —
//! evaluated via `eval_typed_expr`, no bridge/NullCatalog needed.

use super::super::dml;
use super::super::trigger_queue::TriggerOp;
use super::super::trigger_worker;
use super::super::triggers;
use super::super::ExecuteResult;
use super::core::Executor;
use crate::sql::analyzer::types::{
    AnalyzedDelete, AnalyzedInsert, AnalyzedInsertSource, AnalyzedOnConflict, AnalyzedProjection,
    AnalyzedUpdate, TypedExpr, TypedExprKind,
};
use crate::sql::check_constraints;
use crate::sql::dml::ConflictBehavior;
use crate::sql::expr::static_eval::needs_async_materialization;
use crate::sql::expr::typed_eval::eval_typed_expr;
use crate::sql::expr::typed_fold::fold_typed_expr;
use crate::sql::expr::typed_rewrite::materialize_sequences_in_typed_expr;
use crate::sql::query_context::QueryContext;
use crate::types::{Row, TableSchema, Value};
use anyhow::{anyhow, Result};
use std::collections::HashMap;
use tikv_client::Transaction;

impl Executor {
    // ── DELETE (analyzed) ───────────────────────────────────

    pub(crate) async fn execute_analyzed_delete(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        del: &AnalyzedDelete,
    ) -> Result<ExecuteResult> {
        let t = &del.table_name;
        let schema = self
            .store()
            .get_schema(txn, db_id, t)
            .await?
            .ok_or_else(|| anyhow!("Table '{}' does not exist", t))?;
        let trigger_defs = self.store().list_triggers_for_table(txn, db_id, t).await?;

        if schema.pk_indices.is_empty() {
            return Err(anyhow!("No PK"));
        }

        let qctx = QueryContext::from_task_locals();
        let folded_where = del.where_clause.as_ref().map(|e| fold_typed_expr(e, &qctx));
        let rows = self.scan_and_fill(txn, db_id, t, &schema).await?;
        let mut cnt = 0;
        let mut ret_rows = Vec::new();
        let ret_cols = build_returning_columns_from_analyzed(&del.returning, &schema);

        // Handle USING clause: scan ALL USING tables and build cross-product rows.
        let using_combined_rows: Option<Vec<Row>> = if !del.using.is_empty() {
            let mut all_table_rows: Vec<Vec<Row>> = Vec::new();
            for using_ref in &del.using {
                let (_name, _schema, rows) = self
                    .resolve_and_scan_table_ref(txn, db_id, search_path, using_ref)
                    .await?;
                all_table_rows.push(rows);
            }
            // Build cross-product of all USING table rows.
            Some(cross_product_rows(&all_table_rows))
        } else {
            None
        };

        for r in &rows {
            let should_delete = if let Some(ref where_expr) = folded_where {
                if let Some(ref using_rows) = using_combined_rows {
                    let mut matched = false;
                    for using_row in using_rows {
                        let combined = combine_rows(r, using_row);
                        let val = eval_typed_expr(where_expr, &combined, &qctx)?;
                        if typed_value_to_bool(val)? {
                            matched = true;
                            break;
                        }
                    }
                    matched
                } else {
                    let val = eval_typed_expr(where_expr, r, &qctx)?;
                    typed_value_to_bool(val)?
                }
            } else {
                if let Some(ref using_rows) = using_combined_rows {
                    !using_rows.is_empty()
                } else {
                    true
                }
            };

            if !should_delete {
                continue;
            }

            // Evaluate RETURNING before delete (row still exists).
            if let Some(ref returning) = del.returning {
                let ret_row = eval_returning_typed(returning, r, &qctx)?;
                ret_rows.push(ret_row);
            }

            dml::execute_delete_row(&self.store(), txn, db_id, t, &schema, r).await?;

            trigger_worker::enqueue_after_triggers(
                txn,
                db_id,
                self.tenant_keyspace(),
                t,
                TriggerOp::Delete,
                Some(r),
                None,
                &trigger_defs,
                &self.store(),
                self,
                sequence_values,
                search_path,
            )
            .await?;

            cnt += 1;
        }

        if del.returning.is_some() {
            let column_types = Some(build_returning_types_from_analyzed(&del.returning, &schema));
            Ok(ExecuteResult::Select {
                column_types,
                columns: ret_cols,
                rows: ret_rows,
                timezone: crate::session_context::current_timezone(),
            })
        } else {
            Ok(ExecuteResult::Delete { affected_rows: cnt })
        }
    }

    // ── UPDATE (analyzed) ───────────────────────────────────

    pub(crate) async fn execute_analyzed_update(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        upd: &AnalyzedUpdate,
    ) -> Result<ExecuteResult> {
        let t = &upd.table_name;
        let schema = self
            .store()
            .get_schema(txn, db_id, t)
            .await?
            .ok_or_else(|| anyhow!("Table '{}' does not exist", t))?;
        let enum_cache = dml::build_enum_label_cache(&self.store(), txn, db_id, &schema).await?;
        let trigger_defs = self.store().list_triggers_for_table(txn, db_id, t).await?;
        let trigger_func_cache = triggers::prefetch_trigger_functions(
            self.trigger_cache(),
            &self.store(),
            txn,
            db_id,
            &trigger_defs,
            "UPDATE",
        )
        .await?;

        if schema.pk_indices.is_empty() {
            return Err(anyhow!("No PK"));
        }

        let qctx = QueryContext::from_task_locals();
        let folded_where = upd.where_clause.as_ref().map(|e| fold_typed_expr(e, &qctx));
        let compiled_checks = check_constraints::compile_check_constraints(&schema, &qctx)?;
        let rows = self.scan_and_fill(txn, db_id, t, &schema).await?;
        let mut cnt = 0;
        let mut ret_rows = Vec::new();
        let ret_cols = build_returning_columns_from_analyzed(&upd.returning, &schema);

        // Handle FROM clause: scan FROM tables.
        // NOTE: Only the first FROM table is supported. Multi-table FROM
        // (e.g. UPDATE t SET ... FROM a, b WHERE ...) requires cross-product
        // logic like DELETE's USING handler. Single FROM table covers the
        // common case; multi-table FROM is a follow-up.
        let from_data = if !upd.from.is_empty() {
            let from_ref = &upd.from[0];
            let (from_name, from_schema, from_rows) = self
                .resolve_and_scan_table_ref(txn, db_id, search_path, from_ref)
                .await?;
            Some((from_name, from_schema, from_rows))
        } else {
            None
        };

        for r in &rows {
            // Find the matching FROM row (if FROM clause exists) and check WHERE.
            // The matched FROM row is used for SET expression evaluation so that
            // column references from the FROM table resolve to the correct row.
            let eval_row = if let Some((_, ref _from_schema, ref from_rows)) = from_data {
                if from_rows.is_empty() {
                    continue;
                } else if let Some(ref where_expr) = folded_where {
                    let mut matched_from = None;
                    for from_row in from_rows {
                        let combined = combine_rows(r, from_row);
                        let val = eval_typed_expr(where_expr, &combined, &qctx)?;
                        if typed_value_to_bool(val)? {
                            matched_from = Some(combined);
                            break;
                        }
                    }
                    match matched_from {
                        Some(row) => row,
                        None => continue, // no FROM row matched WHERE
                    }
                } else {
                    // No WHERE → use first FROM row.
                    combine_rows(r, &from_rows[0])
                }
            } else {
                // No FROM clause — simple WHERE check.
                if let Some(ref where_expr) = folded_where {
                    let val = eval_typed_expr(where_expr, r, &qctx)?;
                    if !typed_value_to_bool(val)? {
                        continue;
                    }
                }
                r.clone()
            };

            let mut new_vals = r.values.clone();
            for (col_idx, ref typed_expr) in &upd.assignments {
                let val = self
                    .eval_assignment_value(
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        &schema,
                        *col_idx,
                        typed_expr,
                        &eval_row,
                        &qctx,
                    )
                    .await?;
                let col = &schema.columns[*col_idx];
                new_vals[*col_idx] = crate::sql::value_coercion::coerce_value_for_column(val, col)?;
            }

            let new_row = Row::new(new_vals);

            // BEFORE triggers.
            let new_row = match triggers::apply_before_triggers_with_cache(
                self.trigger_cache(),
                &self.store(),
                txn,
                db_id,
                sequence_values,
                search_path,
                &trigger_defs,
                &trigger_func_cache,
                &schema,
                "UPDATE",
                new_row,
                Some(r),
            )
            .await?
            {
                Some(row) => row,
                None => continue,
            };

            // Final coercion + check constraints.
            let mut final_vals = new_row.values;
            dml::coerce_row_values(&schema, &mut final_vals)?;
            let new_row = Row::new(final_vals);
            check_constraints::validate_compiled_check_constraints(
                &schema,
                &compiled_checks,
                &new_row,
                &qctx,
            )?;

            // Persist.
            let updated_row = dml::execute_update_row(
                &self.store(),
                txn,
                db_id,
                t,
                &schema,
                r,
                new_row,
                &enum_cache,
            )
            .await?;

            // AFTER triggers.
            trigger_worker::enqueue_after_triggers(
                txn,
                db_id,
                self.tenant_keyspace(),
                t,
                TriggerOp::Update,
                Some(r),
                Some(&updated_row),
                &trigger_defs,
                &self.store(),
                self,
                sequence_values,
                search_path,
            )
            .await?;

            // RETURNING.
            if let Some(ref returning) = upd.returning {
                let ret_row = eval_returning_typed(returning, &updated_row, &qctx)?;
                ret_rows.push(ret_row);
            }

            cnt += 1;
        }

        if upd.returning.is_some() {
            let column_types = Some(build_returning_types_from_analyzed(&upd.returning, &schema));
            Ok(ExecuteResult::Select {
                column_types,
                columns: ret_cols,
                rows: ret_rows,
                timezone: crate::session_context::current_timezone(),
            })
        } else {
            Ok(ExecuteResult::Update { affected_rows: cnt })
        }
    }

    // ── INSERT (analyzed) ───────────────────────────────────

    pub(crate) async fn execute_analyzed_insert(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut HashMap<String, i64>,
        search_path: &[String],
        ins: &AnalyzedInsert,
    ) -> Result<ExecuteResult> {
        let t = &ins.table_name;
        let schema = self
            .store()
            .get_schema(txn, db_id, t)
            .await?
            .ok_or_else(|| anyhow!("Table '{}' does not exist", t))?;
        let enum_cache = dml::build_enum_label_cache(&self.store(), txn, db_id, &schema).await?;
        let trigger_defs = self.store().list_triggers_for_table(txn, db_id, t).await?;
        let trigger_func_cache_insert = triggers::prefetch_trigger_functions(
            self.trigger_cache(),
            &self.store(),
            txn,
            db_id,
            &trigger_defs,
            "INSERT",
        )
        .await?;
        let trigger_func_cache_update = triggers::prefetch_trigger_functions(
            self.trigger_cache(),
            &self.store(),
            txn,
            db_id,
            &trigger_defs,
            "UPDATE",
        )
        .await?;

        let qctx = QueryContext::from_task_locals();
        let folded_on_conflict_where = match &ins.on_conflict {
            Some(AnalyzedOnConflict::DoUpdate { where_clause, .. }) => {
                where_clause.as_ref().map(|e| fold_typed_expr(e, &qctx))
            }
            _ => None,
        };
        let compiled_checks = check_constraints::compile_check_constraints(&schema, &qctx)?;
        let mut affected = 0;
        let mut inserted = 0usize;
        let mut ret_rows = Vec::new();
        let ret_cols = build_returning_columns_from_analyzed(&ins.returning, &schema);

        // Get source rows. Each entry is (values, default_positions) where
        // default_positions tracks which VALUES-list positions used DEFAULT
        // (so fill_defaults_for_row can fill those columns with their defaults).
        let source_rows: Vec<(Vec<Value>, Vec<usize>)> = match &ins.source {
            AnalyzedInsertSource::DefaultValues => {
                // One row of nulls — all columns get defaults.
                vec![(vec![Value::Null; ins.target_columns.len()], vec![])]
            }
            AnalyzedInsertSource::Values(typed_rows) => {
                let mut evaluated = Vec::with_capacity(typed_rows.len());
                let empty_row = Row::new(vec![]);
                for typed_row in typed_rows {
                    let mut vals = Vec::with_capacity(typed_row.len());
                    let mut default_positions = Vec::new();
                    for (i, typed_expr) in typed_row.iter().enumerate() {
                        let folded_expr = fold_typed_expr(typed_expr, &qctx);
                        if is_default_typed_expr(&folded_expr) {
                            // Placeholder for DEFAULT — will be filled below.
                            vals.push(Value::Null);
                            default_positions.push(i);
                        } else {
                            // Materialize sequence calls before sync eval.
                            let materialized = if needs_async_materialization(&folded_expr) {
                                let store = self.store();
                                materialize_sequences_in_typed_expr(
                                    &store,
                                    txn,
                                    db_id,
                                    sequence_values,
                                    search_path,
                                    &folded_expr,
                                    &qctx,
                                )
                                .await?
                            } else {
                                folded_expr
                            };
                            let val = eval_typed_expr(&materialized, &empty_row, &qctx)?;
                            vals.push(val);
                        }
                    }
                    evaluated.push((vals, default_positions));
                }
                evaluated
            }
            AnalyzedInsertSource::Query(ref analyzed_query) => {
                // Execute the analyzed subquery to get result rows.
                let empty_ctes: HashMap<String, (TableSchema, Vec<Row>)> = HashMap::new();
                let result = self
                    .execute_subquery(
                        txn,
                        db_id,
                        sequence_values,
                        search_path,
                        analyzed_query,
                        &empty_ctes,
                    )
                    .await?;
                match result {
                    super::super::ExecuteResult::Select { rows, .. } => {
                        rows.into_iter().map(|r| (r.values, vec![])).collect()
                    }
                    _ => {
                        return Err(anyhow!(
                            "INSERT...SELECT subquery returned non-select result"
                        ))
                    }
                }
            }
        };

        for (source_vals, default_positions) in &source_rows {
            // Build full row: map source values to column positions + fill defaults.
            let mut row_vals = vec![Value::Null; schema.columns.len()];
            for (i, &col_idx) in ins.target_columns.iter().enumerate() {
                if i < source_vals.len() && !default_positions.contains(&i) {
                    row_vals[col_idx] = source_vals[i].clone();
                }
            }

            // Fill defaults for unspecified columns.
            // For DEFAULT VALUES, pass empty slice so fill_missing_columns fills ALL
            // columns with defaults/serials (no columns were explicitly provided).
            // For VALUES with DEFAULT keywords, exclude those positions from fill_columns
            // so fill_missing_columns treats them as unspecified and fills their defaults.
            let fill_columns: Vec<usize> = match &ins.source {
                AnalyzedInsertSource::DefaultValues => vec![],
                _ => ins
                    .target_columns
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| !default_positions.contains(i))
                    .map(|(_, &col_idx)| col_idx)
                    .collect(),
            };
            self.fill_defaults_for_row(
                txn,
                db_id,
                sequence_values,
                search_path,
                &schema,
                &mut row_vals,
                &fill_columns,
            )
            .await?;

            dml::coerce_row_values_allow_null(&schema, &mut row_vals)?;
            let row = Row::new(row_vals);

            // BEFORE INSERT triggers.
            let row = match triggers::apply_before_triggers_with_cache(
                self.trigger_cache(),
                &self.store(),
                txn,
                db_id,
                sequence_values,
                search_path,
                &trigger_defs,
                &trigger_func_cache_insert,
                &schema,
                "INSERT",
                row,
                None,
            )
            .await?
            {
                Some(r) => r,
                None => continue,
            };

            let mut final_vals = row.values;
            dml::coerce_row_values(&schema, &mut final_vals)?;
            let row = Row::new(final_vals);
            check_constraints::validate_compiled_check_constraints(
                &schema,
                &compiled_checks,
                &row,
                &qctx,
            )?;

            let conflict_behavior = match &ins.on_conflict {
                Some(AnalyzedOnConflict::DoNothing) => ConflictBehavior::DoNothing,
                Some(AnalyzedOnConflict::DoUpdate { .. }) => ConflictBehavior::DoUpdate,
                None => ConflictBehavior::Error,
            };
            let result = dml::execute_insert_row(
                &self.store(),
                txn,
                db_id,
                t,
                &schema,
                row,
                conflict_behavior,
                &enum_cache,
            )
            .await?;

            match result {
                dml::InsertRowResult::Inserted(final_row) => {
                    trigger_worker::enqueue_after_triggers(
                        txn,
                        db_id,
                        self.tenant_keyspace(),
                        t,
                        TriggerOp::Insert,
                        None,
                        Some(&final_row),
                        &trigger_defs,
                        &self.store(),
                        self,
                        sequence_values,
                        search_path,
                    )
                    .await?;
                    affected += 1;
                    inserted += 1;
                    if let Some(ref returning) = ins.returning {
                        let ret_row = eval_returning_typed(returning, &final_row, &qctx)?;
                        ret_rows.push(ret_row);
                    }
                }
                dml::InsertRowResult::Skipped => continue,
                dml::InsertRowResult::Conflicted {
                    existing_pk,
                    existing_row,
                    excluded_row,
                } => {
                    // Handle ON CONFLICT DO UPDATE via analyzed expressions.
                    if let Some(ref oc) = ins.on_conflict {
                        match oc {
                            AnalyzedOnConflict::DoNothing => continue,
                            AnalyzedOnConflict::DoUpdate {
                                assignments,
                                where_clause: _,
                            } => {
                                // Build combined row: [existing, excluded].
                                let combined = combine_rows(&existing_row, &excluded_row);

                                // Check WHERE if present.
                                if let Some(ref where_expr) = folded_on_conflict_where {
                                    let val = eval_typed_expr(where_expr, &combined, &qctx)?;
                                    if !typed_value_to_bool(val)? {
                                        continue;
                                    }
                                }

                                // Evaluate SET expressions.
                                let mut updated_vals = existing_row.values.clone();
                                for (col_idx, ref typed_expr) in assignments {
                                    let val = self
                                        .eval_assignment_value(
                                            txn,
                                            db_id,
                                            sequence_values,
                                            search_path,
                                            &schema,
                                            *col_idx,
                                            typed_expr,
                                            &combined,
                                            &qctx,
                                        )
                                        .await?;
                                    let col = &schema.columns[*col_idx];
                                    updated_vals[*col_idx] =
                                        crate::sql::value_coercion::coerce_value_for_column(
                                            val, col,
                                        )?;
                                }
                                let updated_row = Row::new(updated_vals);

                                // BEFORE UPDATE triggers.
                                let updated_row = match triggers::apply_before_triggers_with_cache(
                                    self.trigger_cache(),
                                    &self.store(),
                                    txn,
                                    db_id,
                                    sequence_values,
                                    search_path,
                                    &trigger_defs,
                                    &trigger_func_cache_update,
                                    &schema,
                                    "UPDATE",
                                    updated_row,
                                    Some(&existing_row),
                                )
                                .await?
                                {
                                    Some(row) => row,
                                    None => continue,
                                };

                                let mut final_vals = updated_row.values;
                                dml::coerce_row_values(&schema, &mut final_vals)?;
                                let updated_row = Row::new(final_vals);
                                check_constraints::validate_compiled_check_constraints(
                                    &schema,
                                    &compiled_checks,
                                    &updated_row,
                                    &qctx,
                                )?;

                                let updated_row = if schema.pk_indices.is_empty() {
                                    dml::execute_update_row_by_pk(
                                        &self.store(),
                                        txn,
                                        db_id,
                                        t,
                                        &schema,
                                        &existing_pk,
                                        &existing_row,
                                        updated_row,
                                        &enum_cache,
                                    )
                                    .await?
                                } else {
                                    dml::execute_update_row(
                                        &self.store(),
                                        txn,
                                        db_id,
                                        t,
                                        &schema,
                                        &existing_row,
                                        updated_row,
                                        &enum_cache,
                                    )
                                    .await?
                                };

                                trigger_worker::enqueue_after_triggers(
                                    txn,
                                    db_id,
                                    self.tenant_keyspace(),
                                    t,
                                    TriggerOp::Update,
                                    Some(&existing_row),
                                    Some(&updated_row),
                                    &trigger_defs,
                                    &self.store(),
                                    self,
                                    sequence_values,
                                    search_path,
                                )
                                .await?;

                                affected += 1;
                                if let Some(ref returning) = ins.returning {
                                    let ret_row =
                                        eval_returning_typed(returning, &updated_row, &qctx)?;
                                    ret_rows.push(ret_row);
                                }
                            }
                        }
                    }
                }
            }
        }

        if inserted > 0 {
            self.stats_cache()
                .bump_estimate(db_id, schema.table_id, inserted as isize);
        }

        if ins.returning.is_some() {
            let column_types = Some(build_returning_types_from_analyzed(&ins.returning, &schema));
            Ok(ExecuteResult::Select {
                column_types,
                columns: ret_cols,
                rows: ret_rows,
                timezone: crate::session_context::current_timezone(),
            })
        } else {
            Ok(ExecuteResult::Insert {
                affected_rows: affected,
            })
        }
    }

    // ── Helpers ─────────────────────────────────────────────

    /// Resolve an AnalyzedTableRef to a table name + schema + rows.
    async fn resolve_and_scan_table_ref(
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
    async fn fill_defaults_for_row(
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
    async fn eval_assignment_value(
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

        let materialized = if needs_async_materialization(&folded_expr) {
            let store = self.store();
            materialize_sequences_in_typed_expr(
                &store,
                txn,
                db_id,
                sequence_values,
                search_path,
                &folded_expr,
                qctx,
            )
            .await?
        } else {
            folded_expr
        };
        eval_typed_expr(&materialized, eval_row, qctx)
    }
}

// ── Free helpers ────────────────────────────────────────────

/// Combine two rows (left + right) for join-style evaluation.
fn combine_rows(left: &Row, right: &Row) -> Row {
    let mut values = left.values.clone();
    values.extend(right.values.clone());
    Row::new(values)
}

/// Convert a typed expression result to bool (with NULL → false).
fn typed_value_to_bool(val: Value) -> Result<bool> {
    match val {
        Value::Boolean(b) => Ok(b),
        Value::Null => Ok(false),
        other => Err(anyhow!("WHERE must be boolean, got {:?}", other)),
    }
}

/// Evaluate a RETURNING clause from analyzed projections.
fn eval_returning_typed(
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
fn build_returning_columns_from_analyzed(
    returning: &Option<Vec<AnalyzedProjection>>,
    _schema: &TableSchema,
) -> Vec<String> {
    match returning {
        Some(projs) => projs.iter().map(|p| p.output_name.clone()).collect(),
        None => vec![],
    }
}

/// Build RETURNING column types from analyzed projections.
fn build_returning_types_from_analyzed(
    returning: &Option<Vec<AnalyzedProjection>>,
    _schema: &TableSchema,
) -> Vec<crate::types::DataType> {
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
fn cross_product_rows(table_rows: &[Vec<Row>]) -> Vec<Row> {
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
fn is_default_typed_expr(expr: &TypedExpr) -> bool {
    matches!(expr.kind, TypedExprKind::Default)
}
