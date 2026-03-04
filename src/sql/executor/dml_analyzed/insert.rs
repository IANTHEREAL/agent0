//! Analyzed INSERT execution with ON CONFLICT and RETURNING support.

use super::super::super::dml;
use super::super::super::trigger_worker;
use super::super::super::triggers;
use super::super::super::triggers::queue::TriggerOp;
use super::super::super::ExecuteResult;
use super::super::core::{Executor, PendingHnswMerge};
use super::{
    build_returning_columns_from_analyzed, build_returning_types_from_analyzed, combine_rows,
    eval_returning_typed, is_default_typed_expr, typed_value_to_bool,
};
use crate::model::{Row, TableSchema, Value};
use crate::sql::analyzer::types::{
    AnalyzedConflictTarget, AnalyzedInsert, AnalyzedInsertSource, AnalyzedOnConflict,
};
use crate::sql::check_constraints;
use crate::sql::dml::{ConflictBehavior, ConflictTarget, FkRefSchemaCache};
use crate::sql::expr::typed_fold::fold_typed_expr;
use crate::sql::query_context::QueryContext;
use crate::sql::sequences::SequenceSession;
use anyhow::{anyhow, Result};
use std::collections::HashMap;
use tikv_client::Transaction;

impl Executor {
    // ── INSERT (analyzed) ───────────────────────────────────

    pub(crate) async fn execute_analyzed_insert(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut SequenceSession,
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
        let empty_ctes: HashMap<String, (TableSchema, Vec<Row>)> = HashMap::new();
        let folded_on_conflict_where = match &ins.on_conflict {
            Some(AnalyzedOnConflict::DoUpdate { where_clause, .. }) => {
                where_clause.as_ref().map(|e| fold_typed_expr(e, &qctx))
            }
            _ => None,
        };
        let compiled_checks = check_constraints::compile_check_constraints(&schema, &qctx)?;
        let has_hnsw = schema.indexes.iter().any(|idx| idx.is_hnsw());
        let mut affected = 0;
        let mut inserted = 0usize;
        let mut ret_rows = Vec::new();
        let mut hnsw_inserted_rows: Vec<Row> = Vec::new();
        let mut hnsw_conflict_updates: Vec<(Row, Row)> = Vec::new();
        let ret_cols = build_returning_columns_from_analyzed(&ins.returning, &schema);

        // Get source rows. Each entry is (values, default_positions) where
        // default_positions tracks which VALUES-list positions used DEFAULT
        // (so fill_defaults_for_row can fill those columns with their defaults).
        let source_rows: Vec<(Vec<Value>, Vec<usize>)> = match &ins.source {
            AnalyzedInsertSource::DefaultValues => {
                // One row of nulls -- all columns get defaults.
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
                            // Placeholder for DEFAULT -- will be filled below.
                            vals.push(Value::Null);
                            default_positions.push(i);
                        } else {
                            let val = self
                                .eval_typed_expr_maybe_async(
                                    txn,
                                    db_id,
                                    sequence_values,
                                    search_path,
                                    &folded_expr,
                                    &empty_row,
                                    None,
                                    &empty_ctes,
                                    &qctx,
                                )
                                .await?;
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
                    super::super::super::ExecuteResult::Select { rows, .. } => {
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

        // Build FK ref-schema cache ONCE for the entire statement so that
        // per-row insert/update calls skip redundant get_schema lookups.
        let fk_ref_cache: Option<FkRefSchemaCache> = if !schema.foreign_keys.is_empty() {
            Some(dml::build_fk_ref_schema_cache(&self.store(), txn, db_id, &schema, false).await?)
        } else {
            None
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
                Some(AnalyzedOnConflict::DoUpdate { target, .. }) => {
                    let target = match target {
                        Some(AnalyzedConflictTarget::Columns(cols)) => {
                            Some(ConflictTarget::Columns(cols.clone()))
                        }
                        Some(AnalyzedConflictTarget::Constraint(name)) => {
                            Some(ConflictTarget::Constraint(name.clone()))
                        }
                        None => None,
                    };
                    ConflictBehavior::DoUpdate { target }
                }
                None => ConflictBehavior::Error,
            };
            let result = if has_hnsw {
                dml::execute_insert_row_defer_hnsw(
                    &self.store(),
                    txn,
                    db_id,
                    t,
                    &schema,
                    row,
                    conflict_behavior,
                    &enum_cache,
                    fk_ref_cache.as_ref(),
                )
                .await?
            } else {
                dml::execute_insert_row(
                    &self.store(),
                    txn,
                    db_id,
                    t,
                    &schema,
                    row,
                    conflict_behavior,
                    &enum_cache,
                    fk_ref_cache.as_ref(),
                )
                .await?
            };

            match result {
                dml::InsertRowResult::Inserted(final_row) => {
                    if has_hnsw {
                        hnsw_inserted_rows.push(final_row.clone());
                    }
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
                                target: _,
                            } => {
                                // Build combined row: [existing, excluded].
                                let combined = combine_rows(&existing_row, &excluded_row);

                                // Check WHERE if present.
                                if let Some(ref where_expr) = folded_on_conflict_where {
                                    let val = self
                                        .eval_typed_expr_maybe_async(
                                            txn,
                                            db_id,
                                            sequence_values,
                                            search_path,
                                            where_expr,
                                            &combined,
                                            Some(&schema),
                                            &empty_ctes,
                                            &qctx,
                                        )
                                        .await?;
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

                                let updated_row = if has_hnsw {
                                    // Defer HNSW maintenance to batch after the loop.
                                    let result = if schema.pk_indices.is_empty() {
                                        dml::execute_update_row_by_pk_defer_hnsw(
                                            &self.store(),
                                            txn,
                                            db_id,
                                            t,
                                            &schema,
                                            &existing_pk,
                                            &existing_row,
                                            updated_row,
                                            &enum_cache,
                                            fk_ref_cache.as_ref(),
                                        )
                                        .await?
                                    } else {
                                        dml::execute_update_row_defer_hnsw(
                                            &self.store(),
                                            txn,
                                            db_id,
                                            t,
                                            &schema,
                                            &existing_row,
                                            updated_row,
                                            &enum_cache,
                                            None,
                                            fk_ref_cache.as_ref(),
                                        )
                                        .await?
                                    };
                                    hnsw_conflict_updates
                                        .push((existing_row.clone(), result.clone()));
                                    result
                                } else if schema.pk_indices.is_empty() {
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
                                        fk_ref_cache.as_ref(),
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
                                        None,
                                        fk_ref_cache.as_ref(),
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

        // Batch HNSW maintenance: load graph once, add all inserted
        // vectors, serialize once, write once.  See issue #1284.
        //
        // Visibility contract change vs. pre-batch (per-row) behavior:
        //
        //   Preserved — transaction-level read-your-writes: after this
        //   statement returns, subsequent statements in the same txn see
        //   the updated HNSW graph (batch writes to the same txn buffer).
        //
        //   Changed — intra-statement HNSW graph visibility: previously,
        //   each row wrote the graph to the txn buffer immediately, so a
        //   BEFORE trigger's HNSW scan on row K could see graph updates
        //   from rows 1..K-1.  Now, HNSW graph writes are deferred to
        //   statement end; a BEFORE trigger's HNSW scan sees the
        //   pre-statement graph.
        //
        //   Unaffected — row data visibility: non-HNSW row data is still
        //   written per-row, so triggers can read previously-modified
        //   rows via regular (non-HNSW) queries within the same statement.
        //
        //   Affected surface: only BEFORE triggers (or VALUES subqueries)
        //   that perform HNSW vector search on the same table being
        //   modified.  This is an extremely narrow pattern.
        if !hnsw_inserted_rows.is_empty() {
            let hnsw_stats = dml::batch_maintain_hnsw_indexes_for_inserts(
                txn,
                db_id,
                &schema,
                &hnsw_inserted_rows,
            )
            .await?;
            if hnsw_stats.graph_bytes > 0 {
                self.observability().record_hnsw_serialize(
                    hnsw_stats.graph_bytes,
                    hnsw_stats.serialize_duration_us,
                );
            }
            for &index_id in &hnsw_stats.dirty_index_ids {
                self.push_pending_hnsw_merge(PendingHnswMerge {
                    keyspace: self.tenant_keyspace().to_string(),
                    db_id,
                    table_id: schema.table_id,
                    index_id,
                });
            }
        }

        // Batch HNSW maintenance for ON CONFLICT DO UPDATE rows.
        // Same load-once/serialize-once strategy; separate from the insert
        // batch because batch_maintain_hnsw_indexes needs (old, new) pairs
        // to detect unchanged vectors.
        if !hnsw_conflict_updates.is_empty() {
            let hnsw_stats =
                dml::batch_maintain_hnsw_indexes(txn, db_id, &schema, &hnsw_conflict_updates)
                    .await?;
            if hnsw_stats.graph_bytes > 0 {
                self.observability().record_hnsw_serialize(
                    hnsw_stats.graph_bytes,
                    hnsw_stats.serialize_duration_us,
                );
            }
            for &index_id in &hnsw_stats.dirty_index_ids {
                self.push_pending_hnsw_merge(PendingHnswMerge {
                    keyspace: self.tenant_keyspace().to_string(),
                    db_id,
                    table_id: schema.table_id,
                    index_id,
                });
            }
        }

        if inserted > 0 {
            self.stats_cache()
                .bump_estimate(db_id, schema.table_id, inserted as isize);
            self.stats_cache()
                .bump_mod_count(db_id, schema.table_id, inserted as u64);
            self.maybe_enqueue_auto_analyze(db_id, schema.table_id, t);
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
}
