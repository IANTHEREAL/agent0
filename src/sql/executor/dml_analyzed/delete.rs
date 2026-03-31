//! Analyzed DELETE execution with USING and trigger support.

use super::super::super::dml;
use super::super::super::trigger_worker;
use super::super::super::triggers::queue::TriggerOp;
use super::super::super::ExecuteResult;
use super::super::core::Executor;
use super::{
    append_ctid_to_rows, build_returning_columns_from_analyzed,
    build_returning_types_from_analyzed, check_cross_product_limit, combine_rows,
    cross_product_rows, eval_returning_typed, typed_value_to_bool,
};
use crate::model::{Row, TableSchema};
use crate::sql::analyzer::types::AnalyzedDelete;
use crate::sql::dml::{FkStoreCtx, pk_to_hash_key};
use crate::sql::expr::typed_fold::fold_typed_expr;
use crate::sql::query_context::QueryContext;
use crate::sql::rls::dml::RlsDmlContext;
use crate::sql::sequences::SequenceSession;
use crate::txn::BatchMutation;
use anyhow::{anyhow, Result};
use std::collections::{HashMap, HashSet};
use tikv_client::Transaction;

impl Executor {
    // ── DELETE (analyzed) ───────────────────────────────────

    pub(crate) async fn execute_analyzed_delete(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut SequenceSession,
        search_path: &[String],
        del: &AnalyzedDelete,
        rls_ctx: Option<&RlsDmlContext>,
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
        let mut rows = self.scan_and_fill(txn, db_id, t, &schema).await?;
        append_ctid_to_rows(&mut rows);
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
            let max_rows = super::dml_table_scan_max_rows_from_settings();
            let sizes: Vec<usize> = all_table_rows.iter().map(|rows| rows.len()).collect();
            check_cross_product_limit(&sizes, max_rows)?;
            // Build cross-product of all USING table rows.
            Some(cross_product_rows(&all_table_rows, max_rows)?)
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
            } else if let Some(ref using_rows) = using_combined_rows {
                !using_rows.is_empty()
            } else {
                true
            };

            // RLS: check row visibility through USING policies.
            // Invisible rows are silently skipped (PG semantics).
            if should_delete {
                if let Some(rls) = rls_ctx {
                    if !rls.is_row_visible(r, &qctx)? {
                        continue;
                    }
                }
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

        // ── Phase 2: execute deletions (batch path) ────────────
        //
        // Collect all TiKV keys for the main table, then flush in a
        // single batch_mutate RPC.  This reduces N×(1+I) sequential
        // pessimistic lock RPCs to 1 batch RPC.
        //
        // Deadlock prevention:
        //  - DELETE vs DELETE: batch + sorted keys → same lock order → no deadlock
        //  - UPDATE vs UPDATE: PK sort → same lock order → no deadlock
        //  - DELETE vs UPDATE: different lock strategies (batch-sorted vs per-row)
        //    so ordering alone cannot prevent deadlocks; handled by the
        //    deadlock auto-retry in retry.rs (exponential backoff).
        //
        // FK cascade still runs per-row on child tables (different
        // table, no lock ordering conflict with the main table).
        // HNSW lazy deletion requires IO and runs per-row.

        let fk_store_ctx = FkStoreCtx {
            store: &self.store(),
            db_id,
        };
        let mut all_delete_keys: Vec<Vec<u8>> = Vec::new();

        for r in &rows_to_delete {
            // Evaluate RETURNING before delete (row still exists).
            if let Some(ref returning) = del.returning {
                let ret_row = eval_returning_typed(returning, r, &qctx)?;
                ret_rows.push(ret_row);
            }

            // FK cascade: handle RESTRICT/NO ACTION/CASCADE on child tables.
            // Cascade deletes child rows per-key (different tables, safe).
            dml::handle_foreign_key_on_delete(
                &fk_store_ctx,
                txn,
                t,
                &schema,
                r,
                &stmt_deleting_pks,
                &mut fk_ctx,
            )
            .await?;

            // HNSW lazy deletion: requires IO (meta read + rowid lookup).
            // Runs per-row before batch flush since it needs txn access.
            for index in &schema.indexes {
                if !index.is_hnsw() {
                    continue;
                }
                if matches!(index.state, crate::worker::types::IndexState::Invalid)
                    || (matches!(index.state, crate::worker::types::IndexState::Building)
                        && !index.unique)
                {
                    continue;
                }
                let pk_values = schema.get_pk_values(r);
                let meta_key = crate::sql::hnsw::storage::hnsw_meta_key(
                    db_id,
                    schema.table_id,
                    index.id,
                );
                if let Some(meta_bytes) = txn.get(meta_key).await? {
                    let meta: crate::sql::hnsw::HnswMeta =
                        serde_json::from_slice(&meta_bytes)?;
                    if meta.label_mode == crate::sql::hnsw::HnswLabelMode::Mapped {
                        let pk_bytes = crate::storage::encode_pk_values(&pk_values);
                        if let Some(rowid) = crate::sql::hnsw::storage::get_rowid_for_pk(
                            txn,
                            db_id,
                            schema.table_id,
                            &pk_bytes,
                        )
                        .await?
                        {
                            crate::sql::hnsw::storage::delete_rowid_mapping(
                                txn,
                                db_id,
                                schema.table_id,
                                &pk_bytes,
                                rowid,
                            )
                            .await?;
                        }
                    }
                }
            }

            // Collect non-HNSW deletion keys (data + indexes) for batch flush.
            let keys = dml::collect_deletion_keys(&self.store(), db_id, &schema, r)?;
            all_delete_keys.extend(keys);

            cnt += 1;
        }

        // Batch flush: delete all collected keys in a single RPC.
        // Sort by key bytes so that chunking (for >10K keys) preserves
        // monotonic lock ordering across chunks. This prevents deadlocks
        // between concurrent DELETEs. Cross-DML deadlocks (DELETE vs
        // UPDATE) are handled by the deadlock retry in retry.rs, since
        // UPDATE's per-row lock order differs from DELETE's global sort.
        if !all_delete_keys.is_empty() {
            all_delete_keys.sort();
            let delete_mutations: Vec<BatchMutation> = all_delete_keys
                .into_iter()
                .map(BatchMutation::Delete)
                .collect();
            crate::txn::txn_batch_mutate_mixed(txn, delete_mutations).await?;
        }

        // Enqueue AFTER triggers after all mutations are flushed,
        // so trigger SQL can see all deletions in the txn buffer.
        for r in &rows_to_delete {
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
        }

        // Bump mod_count for auto-ANALYZE tracking.
        if cnt > 0 {
            self.stats_cache()
                .bump_mod_count(db_id, schema.table_id, cnt);
            self.maybe_enqueue_auto_analyze(db_id, schema.table_id, t);
        }

        if del.returning.is_some() {
            // RLS: validate RETURNING rows against SELECT USING policies.
            if let Some(rls) = rls_ctx {
                rls.check_returning(&schema, &ret_rows, &qctx)?;
            }
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
