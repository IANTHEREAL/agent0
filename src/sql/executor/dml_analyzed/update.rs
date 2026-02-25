//! Analyzed UPDATE execution with FROM, triggers, and constraint support.

use super::super::super::dml;
use super::super::super::trigger_worker;
use super::super::super::triggers;
use super::super::super::triggers::queue::TriggerOp;
use super::super::super::ExecuteResult;
use super::super::core::Executor;
use super::{
    append_ctid_to_rows, build_returning_columns_from_analyzed,
    build_returning_types_from_analyzed, combine_rows, cross_product_rows, eval_returning_typed,
    typed_value_to_bool,
};
use crate::model::{Row, TableSchema};
use crate::sql::analyzer::types::AnalyzedUpdate;
use crate::sql::check_constraints;
use crate::sql::expr::typed_fold::fold_typed_expr;
use crate::sql::query_context::QueryContext;
use anyhow::{anyhow, Result};
use std::collections::HashMap;
use tikv_client::Transaction;

impl Executor {
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
        let empty_ctes: HashMap<String, (TableSchema, Vec<Row>)> = HashMap::new();
        let compiled_checks = check_constraints::compile_check_constraints(&schema, &qctx)?;
        let mut rows = self.scan_and_fill(txn, db_id, t, &schema).await?;
        append_ctid_to_rows(&mut rows);
        let mut cnt = 0;
        let mut ret_rows = Vec::new();
        let ret_cols = build_returning_columns_from_analyzed(&upd.returning, &schema);

        // Handle FROM clause: scan ALL FROM tables and build cross-product rows.
        let from_combined_rows: Option<Vec<Row>> = if !upd.from.is_empty() {
            let mut all_table_rows: Vec<Vec<Row>> = Vec::new();
            for from_ref in &upd.from {
                let (_name, _schema, rows) = self
                    .resolve_and_scan_table_ref(txn, db_id, search_path, from_ref)
                    .await?;
                all_table_rows.push(rows);
            }
            Some(cross_product_rows(&all_table_rows))
        } else {
            None
        };

        for r in &rows {
            // Find the matching FROM row (if FROM clause exists) and check WHERE.
            // The matched FROM row is used for SET expression evaluation so that
            // column references from the FROM table resolve to the correct row.
            let eval_row = if let Some(ref from_rows) = from_combined_rows {
                if from_rows.is_empty() {
                    continue;
                } else if let Some(ref where_expr) = folded_where {
                    let mut matched_from = None;
                    for from_row in from_rows {
                        let combined = combine_rows(r, from_row);
                        let val = self
                            .eval_typed_expr_maybe_async(
                                txn,
                                db_id,
                                sequence_values,
                                search_path,
                                where_expr,
                                &combined,
                                None,
                                &empty_ctes,
                                &qctx,
                            )
                            .await?;
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
                    // No WHERE -> use first FROM row.
                    combine_rows(r, &from_rows[0])
                }
            } else {
                // No FROM clause -- simple WHERE check.
                if let Some(ref where_expr) = folded_where {
                    let val = self
                        .eval_typed_expr_maybe_async(
                            txn,
                            db_id,
                            sequence_values,
                            search_path,
                            where_expr,
                            r,
                            Some(&schema),
                            &empty_ctes,
                            &qctx,
                        )
                        .await?;
                    if !typed_value_to_bool(val)? {
                        continue;
                    }
                }
                r.clone()
            };

            // Truncate to schema column count to strip synthetic ctid appended
            // by append_ctid_to_rows — ctid must never be persisted.
            let mut new_vals = r.values[..schema.columns.len()].to_vec();
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
                None,
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

        // Bump mod_count for auto-ANALYZE tracking.
        if cnt > 0 {
            self.stats_cache()
                .bump_mod_count(db_id, schema.table_id, cnt);
            self.maybe_enqueue_auto_analyze(db_id, schema.table_id, t);
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
}
