//! Analyzed DELETE execution with USING and trigger support.

use super::super::super::dml;
use super::super::super::trigger_worker;
use super::super::super::triggers::queue::TriggerOp;
use super::super::super::ExecuteResult;
use super::super::core::Executor;
use super::{
    build_returning_columns_from_analyzed, build_returning_types_from_analyzed, combine_rows,
    cross_product_rows, eval_returning_typed, typed_value_to_bool,
};
use crate::model::{Row, TableSchema};
use crate::sql::analyzer::types::AnalyzedDelete;
use crate::sql::dml::pk_to_hash_key;
use crate::sql::expr::typed_fold::fold_typed_expr;
use crate::sql::query_context::QueryContext;
use anyhow::{anyhow, Result};
use std::collections::{HashMap, HashSet};
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
        let empty_ctes: HashMap<String, (TableSchema, Vec<Row>)> = HashMap::new();
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

        // ── Phase 1: collect rows to delete ────────────────────────
        let mut rows_to_delete: Vec<&Row> = Vec::new();
        for r in &rows {
            let should_delete = if let Some(ref where_expr) = folded_where {
                if let Some(ref using_rows) = using_combined_rows {
                    let mut matched = false;
                    for using_row in using_rows {
                        let combined = combine_rows(r, using_row);
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
                            matched = true;
                            break;
                        }
                    }
                    matched
                } else {
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
                    typed_value_to_bool(val)?
                }
            } else {
                if let Some(ref using_rows) = using_combined_rows {
                    !using_rows.is_empty()
                } else {
                    true
                }
            };

            if should_delete {
                rows_to_delete.push(r);
            }
        }

        // Compute all PKs being deleted by this statement upfront as a
        // HashSet for O(1) membership checks.  Passed to FK handlers so
        // that both NO ACTION and RESTRICT get correct PostgreSQL
        // statement-level semantics: referencing rows that are also being
        // deleted in the same statement are not considered violations.
        let stmt_deleting_pks: HashSet<String> = rows_to_delete
            .iter()
            .map(|r| pk_to_hash_key(&schema.get_pk_values(r)))
            .collect();

        // Build FK context once for the entire statement.
        // Skip expensive metadata/data loads when no rows match DELETE.
        let mut fk_ctx = if rows_to_delete.is_empty() {
            dml::FkDeleteContext::default()
        } else {
            dml::FkDeleteContext::build(&self.store(), txn, db_id).await?
        };

        // ── Phase 2: execute deletions ───────────────────────────
        for r in &rows_to_delete {
            // Evaluate RETURNING before delete (row still exists).
            if let Some(ref returning) = del.returning {
                let ret_row = eval_returning_typed(returning, r, &qctx)?;
                ret_rows.push(ret_row);
            }

            dml::execute_delete_row(
                &self.store(),
                txn,
                db_id,
                t,
                &schema,
                r,
                &stmt_deleting_pks,
                &mut fk_ctx,
            )
            .await?;

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

        // Bump mod_count for auto-ANALYZE tracking.
        if cnt > 0 {
            self.stats_cache()
                .bump_mod_count(db_id, schema.table_id, cnt as u64);
            self.maybe_enqueue_auto_analyze(db_id, schema.table_id, t);
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
}
