//! Analyzed UPDATE execution with FROM, triggers, and constraint support.

use super::super::super::dml;
use super::super::super::trigger_worker;
use super::super::super::triggers;
use super::super::super::triggers::queue::TriggerOp;
use super::super::super::ExecuteResult;
use super::super::core::{Executor, PendingHnswMerge};
use super::{
    append_ctid_to_rows, build_returning_columns_from_analyzed,
    build_returning_types_from_analyzed, check_cross_product_limit, combine_rows,
    cross_product_rows, eval_returning_typed, typed_value_to_bool,
};
use crate::model::{DataType, Row, TableSchema, Value};
use crate::sql::analyzer::types::{AnalyzedUpdate, TypedExpr, TypedExprKind};
use crate::sql::dml::FkStoreCtx;
use crate::sql::error::SqlError;
use crate::sql::expr::typed_fold::fold_typed_expr;
use crate::sql::index_consistency::{resolve_unique_index_conflict, UniqueConflictResolution};
use crate::sql::projection::fill_row_defaults;
use crate::sql::query_context::QueryContext;
use crate::sql::rls::dml::RlsDmlContext;
use crate::sql::sequences::SequenceSession;
use crate::storage::indexes::BatchIndexEntry;
use crate::txn::BatchMutation;
use crate::worker::types::IndexState;
use anyhow::{anyhow, Result};
use std::collections::{HashMap, HashSet};
use tikv_client::Transaction;

impl Executor {
    // ── UPDATE (analyzed) ───────────────────────────────────

    pub(crate) async fn execute_analyzed_update(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        sequence_values: &mut SequenceSession,
        search_path: &[String],
        upd: &AnalyzedUpdate,
        rls_ctx: Option<&RlsDmlContext>,
        ctes: &HashMap<String, (TableSchema, Vec<Row>)>,
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
        let write_plan = self.compile_write_row_plan(&schema, &qctx)?;
        // Fast path: when WHERE targets a PK or unique key, use point-get
        // instead of a full table scan.  Falls back to scan_and_fill for all
        // other predicate shapes (FROM present, WHERE absent, non-strict
        // predicates).  See issue #1284.
        let mut rows = if upd.from.is_empty() {
            if let Some(ref where_expr) = folded_where {
                match self
                    .try_pk_fast_fetch(txn, db_id, &schema, where_expr)
                    .await?
                {
                    Some(fetched) => fetched,
                    None => self.scan_and_fill(txn, db_id, t, &schema).await?,
                }
            } else {
                self.scan_and_fill(txn, db_id, t, &schema).await?
            }
        } else {
            self.scan_and_fill(txn, db_id, t, &schema).await?
        };
        append_ctid_to_rows(&mut rows);
        let has_hnsw = schema.indexes.iter().any(|idx| idx.is_hnsw());
        let mut cnt = 0;
        let mut ret_rows = Vec::new();
        let mut hnsw_changes: Vec<(Row, Row)> = Vec::new();

        // Deferred AFTER triggers: collect (old_row, new_row) per-row,
        // execute after ALL mutations + HNSW maintenance are flushed.
        // Only allocate when AFTER UPDATE triggers exist.
        struct DeferredAfterTrigger {
            old_row: Row,
            new_row: Row,
        }
        let has_after_triggers = trigger_defs.iter().any(|td| {
            td.timing.eq_ignore_ascii_case("AFTER")
                && td.events.iter().any(|e| e.eq_ignore_ascii_case("UPDATE"))
        });
        let mut deferred_triggers: Vec<DeferredAfterTrigger> = Vec::new();
        let ret_cols = build_returning_columns_from_analyzed(&upd.returning, &schema);

        // Handle FROM clause: scan ALL FROM tables and build cross-product rows.
        let from_combined_rows: Option<Vec<Row>> = if !upd.from.is_empty() {
            let mut all_table_rows: Vec<Vec<Row>> = Vec::new();
            for from_ref in &upd.from {
                let (_name, _schema, rows) = self
                    .resolve_and_scan_table_ref(txn, db_id, search_path, from_ref, ctes)
                    .await?;
                all_table_rows.push(rows);
            }
            let max_rows = super::dml_table_scan_max_rows_from_settings();
            let sizes: Vec<usize> = all_table_rows.iter().map(|rows| rows.len()).collect();
            check_cross_product_limit(&sizes, max_rows)?;
            Some(cross_product_rows(&all_table_rows, max_rows)?)
        } else {
            None
        };

        // Build FK ref-schema cache ONCE for the entire statement so that
        // per-row update calls skip redundant get_schema lookups.
        let fk_ref_cache: Option<dml::FkRefSchemaCache> = if !schema.foreign_keys.is_empty() {
            Some(dml::build_fk_ref_schema_cache(&self.store(), txn, db_id, &schema, false).await?)
        } else {
            None
        };

        // Sort rows by primary key to ensure deterministic lock acquisition
        // order.  This prevents pessimistic lock deadlocks when concurrent
        // UPDATE/DELETE statements touch overlapping rows via different index
        // scans — both sessions will lock rows in the same PK order, making
        // circular waits impossible.  See issue #2252.
        //
        // Pre-compute encoded PK keys (Schwartzian transform) to avoid
        // O(N log N) clone+encode overhead in the sort comparator.
        {
            let pk_indices = &schema.pk_indices;
            let mut keyed: Vec<(Vec<u8>, usize)> = rows
                .iter()
                .enumerate()
                .map(|(i, r)| {
                    let pk: Vec<Value> = pk_indices
                        .iter()
                        .map(|&idx| r.values[idx].clone())
                        .collect();
                    (crate::storage::encode_pk_values(&pk), i)
                })
                .collect();
            keyed.sort_unstable_by(|a, b| a.0.cmp(&b.0));
            let sorted_indices: Vec<usize> = keyed.into_iter().map(|(_, i)| i).collect();
            let mut sorted_rows = Vec::with_capacity(rows.len());
            for i in sorted_indices {
                sorted_rows.push(std::mem::replace(&mut rows[i], Row::new(vec![])));
            }
            rows = sorted_rows;
        }

        // Refresh target rows via TiKV's read+lock path before evaluating the
        // update. A plain lock acquired after the initial scan is insufficient:
        // it prevents later writers from overtaking us, but we would still
        // compute new values from a stale pre-lock snapshot (`read old value,
        // then unconditional upsert`), which silently loses updates under
        // concurrency. Re-reading under `batch_get_for_update` closes that
        // gap and gives us the latest committed row image in deterministic PK
        // order.
        let txn_dirty_tables = crate::session_context::current_txn_dirty_table_ids();
        let statement_dirty_tables = crate::session_context::current_statement_dirty_table_ids();
        let table_dirty_in_txn = txn_dirty_tables.contains(&schema.table_id)
            || statement_dirty_tables.contains(&schema.table_id);

        if !rows.is_empty() && !table_dirty_in_txn {
            let pk_list: Vec<Vec<Value>> = rows.iter().map(|r| schema.get_pk_values(r)).collect();
            rows = self
                .store()
                .batch_get_rows_for_update(
                    txn,
                    db_id,
                    schema.table_id,
                    pk_list,
                    &schema,
                    qctx.lock_timeout,
                )
                .await?;
            rows = fill_fetched_rows(rows, &schema)?;
            append_ctid_to_rows(&mut rows);
        }

        // Check whether BEFORE UPDATE triggers exist.  When they do,
        // we must execute per-row (triggers can veto, mutate, or query
        // intermediate txn state).  Without them, we use the batch fast
        // path: collect all mutations, batch unique-check, single
        // batch_mutate RPC.
        let has_before_triggers = trigger_defs.iter().any(|td| {
            td.timing.eq_ignore_ascii_case("BEFORE")
                && td.events.iter().any(|e| e.eq_ignore_ascii_case("UPDATE"))
        });

        // Self-referential FK with ON UPDATE CASCADE: cascade ordering
        // semantics differ between per-row and batch paths, so we must
        // fall back to per-row when the table references itself.
        let has_self_ref_fk = schema.foreign_keys.iter().any(|fk| {
            fk.ref_table == schema.name
                || fk.ref_table == schema.name.rsplit('.').next().unwrap_or(&schema.name)
        });

        if has_before_triggers || has_self_ref_fk {
            // ── Per-row path (unchanged): BEFORE triggers present ──
            for r in &rows {
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
                                    ctes,
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
                            None => continue,
                        }
                    } else {
                        combine_rows(r, &from_rows[0])
                    }
                } else {
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
                                ctes,
                                &qctx,
                            )
                            .await?;
                        if !typed_value_to_bool(val)? {
                            continue;
                        }
                    }
                    r.clone()
                };

                if let Some(rls) = rls_ctx {
                    if !rls.is_row_visible(r, &qctx)? {
                        continue;
                    }
                }

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
                            ctes,
                        )
                        .await?;
                    let col = &schema.columns[*col_idx];
                    new_vals[*col_idx] =
                        crate::sql::types::cast::coerce_value_for_column(val, col)?;
                }
                let new_row = Row::new(new_vals);

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

                let mut final_vals = new_row.values;
                self.finalize_write_row(
                    txn,
                    db_id,
                    sequence_values,
                    search_path,
                    &schema,
                    &write_plan,
                    &mut final_vals,
                    ctes,
                )
                .await?;
                let new_row = Row::new(final_vals);

                if let Some(rls) = rls_ctx {
                    rls.check_row(&schema, &new_row, &qctx)?;
                }

                let updated_row = if has_hnsw {
                    let old_row_snapshot = Row::new(r.values[..schema.columns.len()].to_vec());
                    let result = dml::execute_update_row_defer_hnsw(
                        &self.store(),
                        txn,
                        db_id,
                        t,
                        &schema,
                        r,
                        new_row,
                        &enum_cache,
                        None,
                        fk_ref_cache.as_ref(),
                    )
                    .await?;
                    hnsw_changes.push((old_row_snapshot, result.clone()));
                    result
                } else {
                    dml::execute_update_row(
                        &self.store(),
                        txn,
                        db_id,
                        t,
                        &schema,
                        r,
                        new_row,
                        &enum_cache,
                        None,
                        fk_ref_cache.as_ref(),
                    )
                    .await?
                };

                if has_after_triggers {
                    deferred_triggers.push(DeferredAfterTrigger {
                        old_row: Row::new(r.values[..schema.columns.len()].to_vec()),
                        new_row: updated_row.clone(),
                    });
                }

                if let Some(ref returning) = upd.returning {
                    let ret_row = eval_returning_typed(returning, &updated_row, &qctx)?;
                    ret_rows.push(ret_row);
                }

                cnt += 1;
            }
        } else {
            // ── Batch fast path: no BEFORE triggers ───────────────
            //
            // Collect all mutations per-row (validating constraints
            // inline), then flush in a single batch_mutate RPC.  This
            // reduces N×(1+I) sequential pessimistic lock RPCs to 1
            // batch RPC, matching the batch DELETE approach (#2254).
            //
            // Deadlock prevention: rows are already sorted by PK
            // (Schwartzian transform above).  Batch mutations are
            // sorted by key bytes before flushing to ensure
            // deterministic lock ordering across chunks.

            struct UpdatedRowInfo {
                old_row: Row,
                new_row: Row,
            }
            let mut updated_rows: Vec<UpdatedRowInfo> = Vec::new();
            let mut all_delete_keys: Vec<Vec<u8>> = Vec::new();
            let mut all_new_btree_entries: Vec<BatchIndexEntry> = Vec::new();
            let mut all_new_gin_mutations: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
            let mut all_new_data_mutations: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();

            // Track PK changes within the batch to handle PK-shifting
            // UPDATEs (e.g. `UPDATE t SET id = id - 1`).  Without this,
            // the batch_get_rows check would see the OLD row (not yet
            // deleted) and falsely reject the new PK.
            //
            // vacated_pks: old PKs being freed by rows whose PK changed.
            // claimed_new_pks: new PKs being claimed — catches two rows
            //   targeting the same new PK (P1: silent data loss).
            let mut vacated_pks: HashSet<Vec<u8>> = HashSet::new();
            let mut claimed_new_pks: HashSet<Vec<u8>> = HashSet::new();

            // ── Phase 1: validate + collect mutations per-row ─────
            for (row_offset, r) in rows.iter().enumerate() {
                // WHERE / FROM evaluation (same as trigger path).
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
                                    ctes,
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
                            None => continue,
                        }
                    } else {
                        combine_rows(r, &from_rows[0])
                    }
                } else {
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
                                ctes,
                                &qctx,
                            )
                            .await?;
                        if !typed_value_to_bool(val)? {
                            continue;
                        }
                    }
                    r.clone()
                };

                // RLS USING check.
                if let Some(rls) = rls_ctx {
                    if !rls.is_row_visible(r, &qctx)? {
                        continue;
                    }
                }

                // Compute new row values (SET assignments + coercion).
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
                            ctes,
                        )
                        .await?;
                    let col = &schema.columns[*col_idx];
                    new_vals[*col_idx] =
                        crate::sql::types::cast::coerce_value_for_column(val, col)?;
                }

                // Recompute generated columns (no BEFORE triggers to
                // intercept, so this is deterministic).
                self.finalize_write_row(
                    txn,
                    db_id,
                    sequence_values,
                    search_path,
                    &schema,
                    &write_plan,
                    &mut new_vals,
                    ctes,
                )
                .await?;
                let new_row = Row::new(new_vals);

                // Full row coercion + NOT NULL checks.
                let mut coerced_vals = new_row.values.clone();
                dml::coerce_row_values(&schema, &mut coerced_vals)?;
                let new_row = Row::new(coerced_vals);

                // Validate enum values.
                dml::validate_enum_values(&schema, &new_row, &enum_cache)?;

                // RLS WITH CHECK.
                if let Some(rls) = rls_ctx {
                    rls.check_row(&schema, &new_row, &qctx)?;
                }

                // FK validation (reads parent tables).
                if !schema.foreign_keys.is_empty() {
                    let owned_cache;
                    let ref_cache = match fk_ref_cache.as_ref() {
                        Some(c) => c,
                        None => {
                            owned_cache = dml::build_fk_ref_schema_cache(
                                &self.store(),
                                txn,
                                db_id,
                                &schema,
                                false,
                            )
                            .await?;
                            &owned_cache
                        }
                    };
                    dml::validate_foreign_keys_with_cache(
                        &self.store(),
                        txn,
                        db_id,
                        &schema,
                        &new_row,
                        ref_cache,
                        None,
                    )
                    .await?;
                }

                let old_row_stripped = Row::new(r.values[..schema.columns.len()].to_vec());
                let old_pks = schema.get_pk_values(&old_row_stripped);
                let new_pks = schema.get_pk_values(&new_row);
                let pk_changed = old_pks != new_pks;

                // PK collision check with intra-batch tracking.
                //
                // Handles three cases:
                //  1. Two rows target the same new PK → intra-batch dup → error
                //  2. New PK exists in TiKV but is being vacated by another
                //     row in this batch → not a conflict (will be freed)
                //  3. New PK exists in TiKV and is NOT being vacated → real collision
                if pk_changed {
                    let new_pk_key = crate::storage::encode_pk_values(&new_pks);

                    // Case 1: intra-batch duplicate new PK.
                    if !claimed_new_pks.insert(new_pk_key.clone()) {
                        let pk_cols: Vec<_> = schema
                            .pk_indices
                            .iter()
                            .map(|&i| schema.columns[i].name.clone())
                            .collect();
                        let pk_vals: Vec<_> = new_pks.iter().map(|v| format!("{}", v)).collect();
                        let default_pk_name = format!(
                            "{}_pkey",
                            schema.name.rsplit('.').next().unwrap_or(&schema.name)
                        );
                        let pk_constraint_name =
                            schema.pk_constraint_name.clone().unwrap_or(default_pk_name);
                        return Err(SqlError::UniqueViolation {
                            constraint: pk_constraint_name.clone(),
                            message: format!(
                                "duplicate key value violates unique constraint \"{}\"\nDETAIL:  Key ({})=({}) already exists.",
                                pk_constraint_name,
                                pk_cols.join(", "),
                                pk_vals.join(", ")
                            ),
                            row_offset: None,
                        }
                        .into());
                    }

                    // Cases 2 & 3: check TiKV unless another row in the
                    // batch is vacating this PK.
                    if !vacated_pks.contains(&new_pk_key) {
                        let existing = self
                            .store()
                            .batch_get_rows(
                                txn,
                                db_id,
                                schema.table_id,
                                vec![new_pks.clone()],
                                &schema,
                            )
                            .await?;
                        if !existing.is_empty() {
                            let pk_cols: Vec<_> = schema
                                .pk_indices
                                .iter()
                                .map(|&i| schema.columns[i].name.clone())
                                .collect();
                            let pk_vals: Vec<_> =
                                new_pks.iter().map(|v| format!("{}", v)).collect();
                            let default_pk_name = format!(
                                "{}_pkey",
                                schema.name.rsplit('.').next().unwrap_or(&schema.name)
                            );
                            let pk_constraint_name =
                                schema.pk_constraint_name.clone().unwrap_or(default_pk_name);
                            return Err(SqlError::UniqueViolation {
                                constraint: pk_constraint_name.clone(),
                                message: format!(
                                    "duplicate key value violates unique constraint \"{}\"\nDETAIL:  Key ({})=({}) already exists.",
                                    pk_constraint_name,
                                    pk_cols.join(", "),
                                    pk_vals.join(", ")
                                ),
                                row_offset: None,
                            }
                            .into());
                        }
                    }

                    // Track this row's old PK as vacated.
                    let old_pk_key = crate::storage::encode_pk_values(&old_pks);
                    vacated_pks.insert(old_pk_key);
                }

                // Collect old index deletion keys (pure key encoding).
                let old_keys = dml::collect_update_old_keys(
                    &self.store(),
                    db_id,
                    &schema,
                    &old_row_stripped,
                    &new_row,
                    &old_pks,
                    pk_changed,
                )?;
                all_delete_keys.extend(old_keys);

                // Collect new B-tree index entries for batch unique check.
                let btree_entries = dml::collect_update_new_btree_entries(
                    &schema,
                    &old_row_stripped,
                    &new_row,
                    &new_pks,
                    pk_changed,
                    row_offset,
                )?;
                all_new_btree_entries.extend(btree_entries);

                // Collect new GIN mutations.
                let gin_mutations = dml::collect_update_new_gin_mutations(
                    &self.store(),
                    db_id,
                    &schema,
                    &old_row_stripped,
                    &new_row,
                    &new_pks,
                    pk_changed,
                )?;
                all_new_gin_mutations.extend(gin_mutations);

                // Collect data row mutation.
                let data_mutation = dml::encode_data_row_mutation(
                    &self.store(),
                    db_id,
                    &schema,
                    &new_pks,
                    &new_row,
                )?;
                all_new_data_mutations.push(data_mutation);

                // Track for HNSW, FK cascade, AFTER triggers, RETURNING.
                if has_hnsw {
                    hnsw_changes.push((old_row_stripped.clone(), new_row.clone()));
                }
                updated_rows.push(UpdatedRowInfo {
                    old_row: old_row_stripped,
                    new_row,
                });
                cnt += 1;
            }

            // ── Phase 2: batch unique constraint check ────────────
            //
            // Uses create_index_entries_batch which does a single
            // batch_get for all unique keys + intra-batch dup check.
            // Pass old_delete_key_set so that unique keys being freed
            // by this batch are not treated as conflicts (handles
            // value-swap UPDATEs like `UPDATE t SET email = 'b' WHERE
            // email = 'a'` alongside another row doing the reverse).
            // On remaining conflicts, attempt resolve_unique_index_conflict
            // (handles stale entries), same as per-row path.
            let old_delete_key_set: HashSet<Vec<u8>> = all_delete_keys.iter().cloned().collect();
            let mut index_kv_mutations: Vec<(Vec<u8>, Vec<u8>)> = Vec::new();
            if !all_new_btree_entries.is_empty() {
                let mut pending = all_new_btree_entries;
                loop {
                    match self
                        .store()
                        .create_index_entries_batch(
                            txn,
                            db_id,
                            schema.table_id,
                            &pending,
                            Some(&old_delete_key_set),
                        )
                        .await
                    {
                        Ok(mutations) => {
                            index_kv_mutations = mutations;
                            break;
                        }
                        Err(err) => {
                            // Try to resolve unique constraint conflict
                            // (stale/idempotent entry).
                            let Some((conflict_constraint, conflict_row_offset)) = err
                                .downcast_ref::<SqlError>()
                                .and_then(|sql_err| match sql_err {
                                    SqlError::UniqueViolation {
                                        constraint,
                                        row_offset: Some(ro),
                                        ..
                                    } => Some((constraint.clone(), *ro)),
                                    _ => None,
                                })
                            else {
                                return Err(err);
                            };

                            let Some(conflict_entry) = pending
                                .iter()
                                .find(|e| {
                                    e.constraint_name == conflict_constraint
                                        && e.row_offset == conflict_row_offset
                                })
                                .cloned()
                            else {
                                return Err(err);
                            };

                            let Some(index) = schema
                                .indexes
                                .iter()
                                .find(|idx| idx.id == conflict_entry.index_id)
                            else {
                                return Err(err);
                            };

                            match resolve_unique_index_conflict(
                                &self.store(),
                                txn,
                                db_id,
                                &schema,
                                index,
                                &conflict_entry.idx_values,
                                &conflict_entry.pk_values,
                            )
                            .await?
                            {
                                UniqueConflictResolution::Idempotent
                                | UniqueConflictResolution::StaleReplaced => {
                                    pending.retain(|e| {
                                        !(e.constraint_name == conflict_constraint
                                            && e.row_offset == conflict_row_offset)
                                    });
                                    if pending.is_empty() {
                                        break;
                                    }
                                }
                                UniqueConflictResolution::RealConflict => {
                                    return Err(err);
                                }
                            }
                        }
                    }
                }
            }

            // ── Phase 3: batch flush all mutations ────────────────
            let mut all_mutations: Vec<BatchMutation> = Vec::new();

            for key in all_delete_keys {
                all_mutations.push(BatchMutation::Delete(key));
            }
            for (key, value) in all_new_data_mutations {
                all_mutations.push(BatchMutation::Put(key, value));
            }
            for (key, value) in index_kv_mutations {
                all_mutations.push(BatchMutation::Put(key, value));
            }
            for (key, value) in all_new_gin_mutations {
                all_mutations.push(BatchMutation::Put(key, value));
            }

            if !all_mutations.is_empty() {
                // Sort by key bytes for deterministic chunk ordering,
                // preventing deadlocks between concurrent batch ops.
                all_mutations.sort_by(|a, b| a.key().cmp(b.key()));
                crate::txn::txn_batch_mutate_mixed(txn, all_mutations).await?;
            }

            // ── Phase 4: FK cascade per-row ───────────────────────
            // FK cascade updates child tables (different table), safe
            // to run after batch flush.  Main table mutations are
            // visible in the txn buffer.
            let fk_store_ctx = FkStoreCtx {
                store: &self.store(),
                db_id,
            };
            for info in &updated_rows {
                dml::handle_foreign_key_on_update(
                    &fk_store_ctx,
                    txn,
                    t,
                    &schema,
                    &info.old_row,
                    &info.new_row,
                    None,
                )
                .await?;
            }

            // ── Phase 5: collect deferred AFTER triggers + RETURNING ──
            for info in &updated_rows {
                if has_after_triggers {
                    deferred_triggers.push(DeferredAfterTrigger {
                        old_row: info.old_row.clone(),
                        new_row: info.new_row.clone(),
                    });
                }

                if let Some(ref returning) = upd.returning {
                    let ret_row = eval_returning_typed(returning, &info.new_row, &qctx)?;
                    ret_rows.push(ret_row);
                }
            }
        }

        // Batch HNSW maintenance: load graph once, add all changed
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
        //   Affected surface: only BEFORE triggers (or SET subqueries)
        //   that perform HNSW vector search on the same table being
        //   modified.  This is an extremely narrow pattern.
        if !hnsw_changes.is_empty() {
            let hnsw_stats =
                dml::batch_maintain_hnsw_indexes(txn, &self.store(), db_id, &schema, &hnsw_changes)
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

        // Execute deferred AFTER triggers now that all mutations
        // (data rows + indexes + HNSW) are flushed.  Each trigger
        // invocation sees the final post-statement table state,
        // matching PostgreSQL semantics.
        for dt in &deferred_triggers {
            trigger_worker::enqueue_after_triggers(
                txn,
                db_id,
                self.tenant_keyspace(),
                t,
                TriggerOp::Update,
                Some(&dt.old_row),
                Some(&dt.new_row),
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

        if upd.returning.is_some() {
            // RLS: validate RETURNING rows against SELECT USING policies.
            if let Some(rls) = rls_ctx {
                rls.check_returning(&schema, &ret_rows, &qctx)?;
            }
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

    // ── PK/Unique-key fast-path helpers ──────────────────────

    /// Attempt point-get fetch when WHERE targets PK or a unique index.
    ///
    /// Supports three predicate shapes:
    /// 1. AND-conjuncts containing `pk = const` — single/composite PK equality,
    ///    with additional residual predicates evaluated after fetch
    /// 2. `pk IN (c1, c2, ...)` — single-column PK in-list (all constants)
    /// 3. AND-conjuncts covering all columns of a UNIQUE index
    ///
    /// Returns `Some(rows)` when the predicate qualifies for fast fetch,
    /// `None` to fall back to full table scan.
    pub(crate) async fn try_pk_fast_fetch(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        schema: &TableSchema,
        where_expr: &TypedExpr,
    ) -> Result<Option<Vec<Row>>> {
        // --- Path A: AND-tree with usable col = const conjuncts ---
        // Keyed by column_index (not name) to avoid case-folding bugs
        // with quoted identifiers (e.g. "A" vs "a").
        let mut eq_map: HashMap<usize, Value> = HashMap::new();
        if collect_conjunctive_eq_by_col_index(where_expr, &mut eq_map).is_some() {
            // NULL constants disable fast path (col = NULL is UNKNOWN).
            if eq_map.values().any(|v| matches!(v, Value::Null)) {
                return Ok(Some(vec![]));
            }

            match choose_fast_fetch_candidate(schema, &eq_map) {
                Some(FastFetchCandidate::PrimaryKey(pk_values)) => {
                    let rows = self
                        .store()
                        .batch_get_rows(txn, db_id, schema.table_id, vec![pk_values], schema)
                        .await?;
                    return Ok(Some(fill_fetched_rows(rows, schema)?));
                }
                Some(FastFetchCandidate::UniqueIndex {
                    index_id,
                    index_values,
                }) => {
                    let pk_types: Vec<DataType> = schema
                        .pk_indices
                        .iter()
                        .map(|&i| schema.columns[i].data_type.clone())
                        .collect();
                    let pk_list = self
                        .store()
                        .scan_index(
                            txn,
                            db_id,
                            schema.table_id,
                            index_id,
                            &index_values,
                            true,
                            &pk_types,
                            None,
                        )
                        .await?;
                    if pk_list.is_empty() {
                        return Ok(Some(vec![]));
                    }
                    let rows = self
                        .store()
                        .batch_get_rows(txn, db_id, schema.table_id, pk_list, schema)
                        .await?;
                    return Ok(Some(fill_fetched_rows(rows, schema)?));
                }
                Some(FastFetchCandidate::PrimaryKeyPrefix(pk_prefix_values)) => {
                    let rows = self
                        .store()
                        .scan_rows_by_pk_prefix(
                            txn,
                            db_id,
                            schema.table_id,
                            &pk_prefix_values,
                            None,
                        )
                        .await?;
                    return Ok(Some(fill_fetched_rows(rows, schema)?));
                }
                None => {}
            }

            // Equalities don't cover PK or any qualifying unique index.
            if !eq_map.is_empty() {
                return Ok(None);
            }
        }

        // --- Path B: pk IN (c1, c2, ...) for single-column PK ---
        if schema.pk_indices.len() == 1 {
            let pk_col_idx = schema.pk_indices[0];
            if let TypedExprKind::InList {
                expr,
                list,
                negated: false,
            } = &where_expr.kind
            {
                if let TypedExprKind::ColumnRef { column_index, .. } = &expr.kind {
                    if *column_index == pk_col_idx {
                        // All list elements must be constants.
                        let values: Vec<Value> = list
                            .iter()
                            .filter_map(|e| {
                                if let TypedExprKind::Constant(v) = &e.kind {
                                    Some(v.clone())
                                } else {
                                    None
                                }
                            })
                            .collect();
                        if values.len() != list.len() {
                            return Ok(None);
                        }

                        // Deduplicate (avoid updating same row twice) and
                        // filter NULLs (IN with NULL never matches).
                        let mut deduped: Vec<Vec<Value>> = Vec::new();
                        for v in values {
                            if matches!(v, Value::Null) {
                                continue;
                            }
                            let pk_vec = vec![v];
                            if !deduped.contains(&pk_vec) {
                                deduped.push(pk_vec);
                            }
                        }

                        if deduped.is_empty() {
                            return Ok(Some(vec![]));
                        }
                        let rows = self
                            .store()
                            .batch_get_rows(txn, db_id, schema.table_id, deduped, schema)
                            .await?;
                        return Ok(Some(fill_fetched_rows(rows, schema)?));
                    }
                }
            }
        }

        Ok(None)
    }
}

#[derive(Debug, PartialEq)]
enum FastFetchCandidate {
    PrimaryKey(Vec<Value>),
    UniqueIndex {
        index_id: u64,
        index_values: Vec<Value>,
    },
    PrimaryKeyPrefix(Vec<Value>),
}

fn choose_fast_fetch_candidate(
    schema: &TableSchema,
    eq_map: &HashMap<usize, Value>,
) -> Option<FastFetchCandidate> {
    if schema.pk_indices.iter().all(|i| eq_map.contains_key(i)) {
        let pk_values = schema
            .pk_indices
            .iter()
            .map(|i| eq_map[i].clone())
            .collect();
        return Some(FastFetchCandidate::PrimaryKey(pk_values));
    }

    if let Some((index_id, index_values)) = unique_index_values_from_eq_map(schema, eq_map) {
        return Some(FastFetchCandidate::UniqueIndex {
            index_id,
            index_values,
        });
    }

    pk_prefix_values_from_eq_map(schema, eq_map).map(FastFetchCandidate::PrimaryKeyPrefix)
}

/// Return values for a fully-covered unique index. This intentionally runs
/// before PK-prefix range fetch so `tenant_id = ? AND email = ?` prefers the
/// unique email point lookup over scanning every row in the tenant prefix.
fn unique_index_values_from_eq_map(
    schema: &TableSchema,
    eq_map: &HashMap<usize, Value>,
) -> Option<(u64, Vec<Value>)> {
    for idx in &schema.indexes {
        if !idx.unique
            || !matches!(idx.state, IndexState::Ready)
            || !idx.expressions.is_empty()
            || idx.predicate.is_some()
        {
            continue;
        }

        let Some(idx_col_indices) = idx
            .columns
            .iter()
            .map(|c| schema.column_index(c))
            .collect::<Option<Vec<_>>>()
        else {
            continue;
        };

        if idx_col_indices.iter().all(|i| eq_map.contains_key(i)) {
            let idx_values = idx_col_indices.iter().map(|i| eq_map[i].clone()).collect();
            return Some((idx.id, idx_values));
        }
    }

    None
}

/// Return equality values for a non-empty, non-complete leading primary-key
/// prefix. Complete PK predicates are handled by point-get before this helper.
fn pk_prefix_values_from_eq_map(
    schema: &TableSchema,
    eq_map: &HashMap<usize, Value>,
) -> Option<Vec<Value>> {
    if schema.pk_indices.len() < 2 {
        return None;
    }

    let mut values = Vec::new();
    for pk_idx in &schema.pk_indices {
        match eq_map.get(pk_idx) {
            Some(value) => values.push(value.clone()),
            None => break,
        }
    }

    if values.is_empty() || values.len() == schema.pk_indices.len() {
        None
    } else {
        Some(values)
    }
}

/// Extract top-level AND-conjunct `col = const` predicates keyed by
/// `column_index` (from `ColumnRef`).
///
/// Unlike `collect_typed_eq_predicates` (which keys by lowercased name),
/// this avoids case-folding bugs with quoted identifiers (e.g. columns
/// `"A"` and `"a"` are distinct in PostgreSQL but would collide under
/// `to_lowercase()`).
///
/// Non-equality conjuncts are left as residual filters and evaluated after
/// fetch. This allows `UPDATE ... WHERE pk = ? AND old_col IS NOT DISTINCT
/// FROM ?` to point-fetch and lock only the target PK row while preserving the
/// original compare-and-swap predicate semantics.
///
/// Returns `None` if the same column index appears with conflicting equality
/// values.
fn collect_conjunctive_eq_by_col_index(
    expr: &TypedExpr,
    out: &mut HashMap<usize, Value>,
) -> Option<()> {
    use crate::sql::analyzer::types::BinaryOp as TypedBinaryOp;

    match &expr.kind {
        TypedExprKind::BinaryOp { left, op, right } => match op {
            TypedBinaryOp::And => {
                collect_conjunctive_eq_by_col_index(left, out)?;
                collect_conjunctive_eq_by_col_index(right, out)?;
                Some(())
            }
            TypedBinaryOp::Eq => {
                let (col_idx, val) =
                    if let TypedExprKind::ColumnRef { column_index, .. } = &left.kind {
                        if let TypedExprKind::Constant(v) = &right.kind {
                            (*column_index, v.clone())
                        } else {
                            return None;
                        }
                    } else if let TypedExprKind::ColumnRef { column_index, .. } = &right.kind {
                        if let TypedExprKind::Constant(v) = &left.kind {
                            (*column_index, v.clone())
                        } else {
                            return None;
                        }
                    } else {
                        return None;
                    };

                if let Some(existing) = out.get(&col_idx) {
                    if existing != &val {
                        return None;
                    }
                    return Some(());
                }
                out.insert(col_idx, val);
                Some(())
            }
            _ => Some(()),
        },
        _ => Some(()),
    }
}

/// Fill defaults on rows returned by `batch_get_rows`, matching the
/// behavior of `scan_and_fill` for rows with fewer columns than the
/// current schema (e.g. columns added after the row was written).
fn fill_fetched_rows(rows: Vec<Row>, schema: &TableSchema) -> Result<Vec<Row>> {
    let mut filled = Vec::with_capacity(rows.len());
    for mut row in rows {
        fill_row_defaults(&mut row, schema)?;
        filled.push(row);
    }
    Ok(filled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ColumnDef, IndexDef};
    use crate::sql::analyzer::types::BinaryOp as TypedBinaryOp;

    fn col(idx: usize, name: &str) -> TypedExpr {
        TypedExpr {
            kind: TypedExprKind::ColumnRef {
                scope_depth: 0,
                column_index: idx,
                column_name: name.to_string(),
            },
            data_type: DataType::Int32,
        }
    }

    fn int(value: i32) -> TypedExpr {
        TypedExpr {
            kind: TypedExprKind::Constant(Value::Int32(value)),
            data_type: DataType::Int32,
        }
    }

    fn bin(left: TypedExpr, op: TypedBinaryOp, right: TypedExpr) -> TypedExpr {
        TypedExpr {
            kind: TypedExprKind::BinaryOp {
                left: Box::new(left),
                op,
                right: Box::new(right),
            },
            data_type: DataType::Boolean,
        }
    }

    fn composite_pk_schema() -> TableSchema {
        TableSchema::new(
            "order_line".to_string(),
            1,
            vec![
                ColumnDef::new("ol_w_id", DataType::Int32, false).primary_key(),
                ColumnDef::new("ol_d_id", DataType::Int32, false).primary_key(),
                ColumnDef::new("ol_o_id", DataType::Int32, false).primary_key(),
                ColumnDef::new("ol_number", DataType::Int32, false).primary_key(),
            ],
            vec![0, 1, 2, 3],
        )
    }

    fn tenant_user_schema_with_unique_email() -> TableSchema {
        let mut schema = TableSchema::new(
            "users".to_string(),
            1,
            vec![
                ColumnDef::new("tenant_id", DataType::Int32, false).primary_key(),
                ColumnDef::new("id", DataType::Int32, false).primary_key(),
                ColumnDef::new("email", DataType::Text, false),
            ],
            vec![0, 1],
        );
        schema.indexes = vec![IndexDef {
            id: 77,
            name: "users_email_key".to_string(),
            columns: vec!["email".to_string()],
            unique: true,
            is_constraint: false,
            method: None,
            predicate: None,
            expressions: Vec::new(),
            state: IndexState::Ready,
            cached_predicate_conjuncts: None,
            hnsw_m: None,
            hnsw_ef_construction: None,
            hnsw_distance_metric: None,
        }];
        schema
    }

    #[test]
    fn collect_conjunctive_eq_keeps_pk_equalities_with_residual_cas_predicates() {
        let pk_eq = bin(col(0, "id"), TypedBinaryOp::Eq, int(42));
        let residual = TypedExpr {
            kind: TypedExprKind::IsDistinctFrom {
                left: Box::new(col(1, "old_balance")),
                right: Box::new(int(7)),
                negated: true,
            },
            data_type: DataType::Boolean,
        };
        let predicate = bin(pk_eq, TypedBinaryOp::And, residual);

        let mut eq_map = HashMap::new();
        collect_conjunctive_eq_by_col_index(&predicate, &mut eq_map).unwrap();

        assert_eq!(eq_map.get(&0), Some(&Value::Int32(42)));
        assert_eq!(eq_map.len(), 1);
    }

    #[test]
    fn collect_conjunctive_eq_does_not_extract_from_or_residuals() {
        let or_predicate = bin(
            bin(col(0, "id"), TypedBinaryOp::Eq, int(42)),
            TypedBinaryOp::Or,
            bin(col(0, "id"), TypedBinaryOp::Eq, int(43)),
        );
        let predicate = bin(
            bin(col(1, "tenant_id"), TypedBinaryOp::Eq, int(9)),
            TypedBinaryOp::And,
            or_predicate,
        );

        let mut eq_map = HashMap::new();
        collect_conjunctive_eq_by_col_index(&predicate, &mut eq_map).unwrap();

        assert_eq!(eq_map.get(&1), Some(&Value::Int32(9)));
        assert!(!eq_map.contains_key(&0));
    }

    #[test]
    fn pk_prefix_values_extracts_leading_non_complete_prefix() {
        let schema = composite_pk_schema();
        let mut eq_map = HashMap::new();
        eq_map.insert(0, Value::Int32(1));
        eq_map.insert(1, Value::Int32(2));
        eq_map.insert(2, Value::Int32(3001));

        assert_eq!(
            pk_prefix_values_from_eq_map(&schema, &eq_map),
            Some(vec![Value::Int32(1), Value::Int32(2), Value::Int32(3001)])
        );
    }

    #[test]
    fn pk_prefix_values_rejects_non_leading_or_complete_pk() {
        let schema = composite_pk_schema();

        let mut non_leading = HashMap::new();
        non_leading.insert(1, Value::Int32(2));
        non_leading.insert(2, Value::Int32(3001));
        assert!(pk_prefix_values_from_eq_map(&schema, &non_leading).is_none());

        let mut complete = HashMap::new();
        complete.insert(0, Value::Int32(1));
        complete.insert(1, Value::Int32(2));
        complete.insert(2, Value::Int32(3001));
        complete.insert(3, Value::Int32(7));
        assert!(pk_prefix_values_from_eq_map(&schema, &complete).is_none());
    }

    #[test]
    fn fast_fetch_candidate_prefers_unique_point_get_over_pk_prefix_scan() {
        let schema = tenant_user_schema_with_unique_email();
        let mut eq_map = HashMap::new();
        eq_map.insert(0, Value::Int32(42));
        eq_map.insert(2, Value::Text("a@example.com".to_string()));

        assert_eq!(
            pk_prefix_values_from_eq_map(&schema, &eq_map),
            Some(vec![Value::Int32(42)]),
            "test setup should also qualify for a potentially large PK-prefix scan"
        );
        assert_eq!(
            choose_fast_fetch_candidate(&schema, &eq_map),
            Some(FastFetchCandidate::UniqueIndex {
                index_id: 77,
                index_values: vec![Value::Text("a@example.com".to_string())],
            })
        );
    }
}
