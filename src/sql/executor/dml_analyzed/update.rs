//! Analyzed UPDATE execution with FROM, triggers, and constraint support.

use super::super::super::dml;
use super::super::super::trigger_worker;
use super::super::super::triggers;
use super::super::super::triggers::queue::TriggerOp;
use super::super::super::ExecuteResult;
use super::super::core::{Executor, PendingHnswMerge};
use super::{
    append_ctid_to_rows, build_returning_columns_from_analyzed,
    build_returning_types_from_analyzed, combine_rows, cross_product_rows, eval_returning_typed,
    typed_value_to_bool,
};
use crate::model::{DataType, Row, TableSchema, Value};
use crate::sql::analyzer::types::{AnalyzedUpdate, TypedExpr, TypedExprKind};
use crate::sql::check_constraints;
use crate::sql::expr::typed_fold::fold_typed_expr;
use crate::sql::projection::fill_row_defaults;
use crate::sql::query_context::QueryContext;
use crate::sql::sequences::SequenceSession;
use crate::worker::types::IndexState;
use anyhow::{anyhow, Result};
use std::collections::HashMap;
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

        // Build FK ref-schema cache ONCE for the entire statement so that
        // per-row update calls skip redundant get_schema lookups.
        let fk_ref_cache: Option<dml::FkRefSchemaCache> = if !schema.foreign_keys.is_empty() {
            Some(dml::build_fk_ref_schema_cache(&self.store(), txn, db_id, &schema, false).await?)
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

            // Persist (defer HNSW maintenance to batch after the loop).
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
                dml::batch_maintain_hnsw_indexes(txn, db_id, &schema, &hnsw_changes).await?;
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

    // ── PK/Unique-key fast-path helpers ──────────────────────

    /// Attempt point-get fetch when WHERE targets PK or a unique index.
    ///
    /// Supports three strict predicate shapes:
    /// 1. `pk = const` — single/composite PK equality
    /// 2. `pk IN (c1, c2, ...)` — single-column PK in-list (all constants)
    /// 3. AND-connected `col = const` covering all columns of a UNIQUE index
    ///
    /// Returns `Some(rows)` when the predicate qualifies for fast fetch,
    /// `None` to fall back to full table scan.
    async fn try_pk_fast_fetch(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        schema: &TableSchema,
        where_expr: &TypedExpr,
    ) -> Result<Option<Vec<Row>>> {
        // --- Path A: pure AND-tree of col = const ---
        // Keyed by column_index (not name) to avoid case-folding bugs
        // with quoted identifiers (e.g. "A" vs "a").
        let mut eq_map: HashMap<usize, Value> = HashMap::new();
        if collect_eq_by_col_index(where_expr, &mut eq_map).is_some() {
            // NULL constants disable fast path (col = NULL is UNKNOWN).
            if eq_map.values().any(|v| matches!(v, Value::Null)) {
                return Ok(None);
            }

            // Check if all PK columns are covered → single PK point-get.
            if schema.pk_indices.iter().all(|i| eq_map.contains_key(i)) {
                let pk_values: Vec<Value> = schema
                    .pk_indices
                    .iter()
                    .map(|i| eq_map[i].clone())
                    .collect();
                let rows = self
                    .store()
                    .batch_get_rows(txn, db_id, schema.table_id, vec![pk_values], schema)
                    .await?;
                return Ok(Some(fill_fetched_rows(rows, schema)?));
            }

            // Check unique indexes (only Ready, pure-column, non-partial,
            // non-expression indexes qualify).
            for idx in &schema.indexes {
                if !idx.unique
                    || !matches!(idx.state, IndexState::Ready)
                    || !idx.expressions.is_empty()
                    || idx.predicate.is_some()
                {
                    continue;
                }
                // Resolve index column names to schema column indices.
                let idx_col_indices: Vec<usize> = idx
                    .columns
                    .iter()
                    .filter_map(|c| schema.column_index(c))
                    .collect();
                if idx_col_indices.len() != idx.columns.len() {
                    continue; // unresolvable column — skip
                }
                if idx_col_indices.iter().all(|i| eq_map.contains_key(i)) {
                    let idx_values: Vec<Value> =
                        idx_col_indices.iter().map(|i| eq_map[i].clone()).collect();
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
                            idx.id,
                            &idx_values,
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
            }

            // Equalities don't cover PK or any qualifying unique index.
            return Ok(None);
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

/// Extract a conjunction of `col = const` predicates keyed by
/// `column_index` (from `ColumnRef`).
///
/// Unlike `collect_typed_eq_predicates` (which keys by lowercased name),
/// this avoids case-folding bugs with quoted identifiers (e.g. columns
/// `"A"` and `"a"` are distinct in PostgreSQL but would collide under
/// `to_lowercase()`).
///
/// Returns `None` if the expression is not a pure AND tree of equalities,
/// or if the same column index appears with conflicting values.
fn collect_eq_by_col_index(expr: &TypedExpr, out: &mut HashMap<usize, Value>) -> Option<()> {
    use crate::sql::analyzer::types::BinaryOp as TypedBinaryOp;

    match &expr.kind {
        TypedExprKind::BinaryOp { left, op, right } => match op {
            TypedBinaryOp::And => {
                collect_eq_by_col_index(left, out)?;
                collect_eq_by_col_index(right, out)?;
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
            _ => None,
        },
        _ => None,
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
