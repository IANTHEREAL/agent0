//! COPY-related helpers

use super::*;

#[derive(Debug)]
pub(crate) struct CopyInsertBatchError {
    failed_row_offset: Option<usize>,
    source: anyhow::Error,
}

impl CopyInsertBatchError {
    fn non_row<E: Into<anyhow::Error>>(error: E) -> Self {
        Self {
            failed_row_offset: None,
            source: error.into(),
        }
    }

    fn row<E: Into<anyhow::Error>>(failed_row_offset: usize, error: E) -> Self {
        Self {
            failed_row_offset: Some(failed_row_offset),
            source: error.into(),
        }
    }

    fn from_storage_batch_error(error: anyhow::Error) -> Self {
        let failed_row_offset =
            error
                .downcast_ref::<SqlError>()
                .and_then(|sql_err| match sql_err {
                    SqlError::UniqueViolation { row_offset, .. } => *row_offset,
                    _ => None,
                });
        match failed_row_offset {
            Some(offset) => Self::row(offset, error),
            None => Self::non_row(error),
        }
    }

    pub(crate) fn failed_row_offset(&self) -> Option<usize> {
        self.failed_row_offset
    }

    pub(crate) fn source_error(&self) -> &anyhow::Error {
        &self.source
    }
}

impl Executor {
    pub fn parse_value_for_copy(&self, val: &str, data_type: &DataType) -> Result<Value> {
        parse_value_for_copy(val, data_type)
    }

    pub(crate) async fn execute_copy_insert_batch(
        &self,
        session: &mut Session,
        table_name: &str,
        rows: Vec<Vec<(String, Value)>>,
        mut accumulated_fk_keys: Option<&mut HashMap<String, HashSet<String>>>,
        mut deferred_fk_checks: Option<&mut Vec<(dml::ConstraintId, String, String)>>,
    ) -> std::result::Result<(), CopyInsertBatchError> {
        if rows.is_empty() {
            return Ok(());
        }

        let is_autocommit = !session.is_in_transaction();

        if is_autocommit {
            session
                .begin()
                .await
                .map_err(CopyInsertBatchError::non_row)?;
        }

        let result = async {
            let db_id = session.current_database_id();
            let (txn, sequence_values, search_path) = session
                .get_mut_txn_sequence_values_and_search_path()
                .ok_or_else(|| anyhow!("Transaction must be active"))
                .map_err(CopyInsertBatchError::non_row)?;
            let schema = self
                .store
                .get_schema(txn, db_id, table_name)
                .await
                .map_err(CopyInsertBatchError::non_row)?
                .ok_or_else(|| SqlError::RelationNotFound(table_name.to_string()))
                .map_err(CopyInsertBatchError::non_row)?;
            let qctx = crate::sql::query_context::QueryContext::from_task_locals();
            let write_plan = self
                .compile_write_row_plan(&schema, &qctx)
                .map_err(CopyInsertBatchError::non_row)?;

            let enum_cache = dml::build_enum_label_cache(&self.store, txn, db_id, &schema)
                .await
                .map_err(CopyInsertBatchError::non_row)?;

            // ── Phase 1: prepare all rows ──────────────────────────────
            // Sequential because fill_missing_columns may need sequence ops.
            let mut prepared_rows: Vec<(Row, usize)> = Vec::with_capacity(rows.len());
            let has_self_ref_fk = schema
                .foreign_keys
                .iter()
                .any(|fk| fk.ref_table == schema.name);
            let mut pending_ref_keys: HashMap<String, HashSet<String>> = HashMap::new();

            for (row_offset, col_values) in rows.into_iter().enumerate() {
                let mut row_values = vec![Value::Null; schema.columns.len()];
                let mut indices: Vec<usize> = Vec::with_capacity(col_values.len());

                for (col_name, value) in col_values {
                    if let Some(idx) = schema.column_index(&col_name) {
                        row_values[idx] = value;
                        indices.push(idx);
                    }
                }
                indices.sort_unstable();
                indices.dedup();
                self.reject_explicit_generated_insert_columns(&schema, &indices)
                    .map_err(|e| CopyInsertBatchError::row(row_offset, e))?;

                dml::fill_missing_columns(
                    &self.store,
                    txn,
                    db_id,
                    sequence_values,
                    search_path,
                    &schema,
                    &mut row_values,
                    &indices,
                )
                .await
                .map_err(|e| CopyInsertBatchError::row(row_offset, e))?;
                self.finalize_write_row(
                    txn,
                    db_id,
                    sequence_values,
                    search_path,
                    &schema,
                    &write_plan,
                    &mut row_values,
                )
                .await
                .map_err(|e| CopyInsertBatchError::row(row_offset, e))?;
                let row = Row { values: row_values };

                // Validate enum values (CPU-only).
                dml::insert::validate_enum_values(&schema, &row, &enum_cache)
                    .map_err(|e| CopyInsertBatchError::row(row_offset, e))?;

                // Collect referenced-column hash keys so that FK validation
                // (second pass) sees ALL rows' PK contributions — matching
                // PostgreSQL's deferred-within-COPY semantics where FK
                // constraints are checked after all rows are processed.
                if has_self_ref_fk {
                    for (fk_name, key) in dml::self_ref_fk_keys(&schema, &row)
                        .map_err(|e| CopyInsertBatchError::row(row_offset, e))?
                    {
                        pending_ref_keys.entry(fk_name).or_default().insert(key);
                    }
                }

                prepared_rows.push((row, row_offset));
            }

            // ── Phase 1b: FK validation ─────────────────────────────────
            // Non-self-referencing FKs: validated immediately (parent rows
            // are in other tables and already visible in storage).
            // Self-referencing FKs: if unresolved in current storage view,
            // defer a compact `(constraint_id, key_string)` check to CopyDone
            // where the complete set of in-COPY parent keys is available.
            let has_non_self_ref_fk = schema
                .foreign_keys
                .iter()
                .any(|fk| fk.ref_table != schema.name);
            if has_non_self_ref_fk {
                let fk_ref_cache =
                    dml::build_fk_ref_schema_cache(&self.store, txn, db_id, &schema, true)
                        .await
                        .map_err(CopyInsertBatchError::non_row)?;
                for (row, row_offset) in &prepared_rows {
                    dml::validate_foreign_keys_non_self_ref(
                        &self.store,
                        txn,
                        db_id,
                        &schema,
                        row,
                        &fk_ref_cache,
                    )
                    .await
                    .map_err(|e| CopyInsertBatchError::row(*row_offset, e))?;
                }
            }
            if has_self_ref_fk {
                if let Some(ref mut deferred) = deferred_fk_checks {
                    for (row, row_offset) in &prepared_rows {
                        let child_vals = dml::collect_deferred_self_fk_checks(
                            &self.store,
                            txn,
                            db_id,
                            &schema,
                            row,
                        )
                        .await
                        .map_err(|e| CopyInsertBatchError::row(*row_offset, e))?;
                        deferred.extend(child_vals);
                    }
                }
            }

            // ── Phase 2: batch PK duplicate check ──────────────────────
            // Single batch_get for all PK keys instead of one get per row.
            // Returns (pk_results, data_mutations) — writes are deferred.
            let (inserted, mut all_mutations) = self
                .store
                .insert_batch(txn, db_id, table_name, &schema, &prepared_rows)
                .await
                .map_err(CopyInsertBatchError::from_storage_batch_error)?;
            let pending_row_keys: HashSet<Vec<u8>> =
                all_mutations.iter().map(|(key, _)| key.clone()).collect();
            let pending_row_key_by_offset: HashMap<usize, Vec<u8>> = inserted
                .iter()
                .zip(all_mutations.iter())
                .map(|((_, row_offset), (key, _))| (*row_offset, key.clone()))
                .collect();

            // ── Phase 3: batch index entry creation ────────────────────
            // Collect B-tree index entries for the whole batch, then check
            // with a single batch_get for the unique-duplicate check.
            use crate::sql::index_consistency::{
                resolve_unique_index_conflict, UniqueConflictResolution,
            };
            use crate::sql::index_helpers;
            use crate::storage::indexes::BatchIndexEntry;
            use crate::worker::types::IndexState;

            let mut pending_index_entries: Vec<BatchIndexEntry> = Vec::new();

            for &(ref pk_values, row_offset) in &inserted {
                let (row, _) = &prepared_rows[row_offset];
                for index in &schema.indexes {
                    if matches!(index.state, IndexState::Invalid)
                        || (matches!(index.state, IndexState::Building) && !index.unique)
                    {
                        continue;
                    }
                    if !index_helpers::is_index_materializable(index) {
                        continue;
                    }
                    if !index_helpers::eval_index_predicate(index, &schema, row)
                        .map_err(|e| CopyInsertBatchError::row(row_offset, e))?
                    {
                        continue;
                    }
                    let idx_values =
                        index_helpers::get_index_values_with_expressions(index, &schema, row)
                            .map_err(|e| CopyInsertBatchError::row(row_offset, e))?;
                    pending_index_entries.push(BatchIndexEntry {
                        index_id: index.id,
                        idx_values,
                        pk_values: pk_values.clone(),
                        unique: index.unique,
                        row_offset,
                        constraint_name: index.name.clone(),
                        key_columns: index.columns.clone(),
                    });
                }
            }

            while !pending_index_entries.is_empty() {
                let batch_result = self
                    .store
                    .create_index_entries_batch(txn, db_id, schema.table_id, &pending_index_entries)
                    .await;
                match batch_result {
                    Ok(index_mutations) => {
                        all_mutations.extend(index_mutations);
                        break;
                    }
                    Err(err) => {
                        let Some((conflict_constraint, conflict_row_offset)) = err
                            .downcast_ref::<SqlError>()
                            .and_then(|sql_err| match sql_err {
                                SqlError::UniqueViolation {
                                    constraint,
                                    row_offset: Some(row_offset),
                                    ..
                                } => Some((constraint.clone(), *row_offset)),
                                _ => None,
                            })
                        else {
                            return Err(CopyInsertBatchError::from_storage_batch_error(err));
                        };

                        let Some(conflict_entry) = pending_index_entries
                            .iter()
                            .find(|entry| {
                                entry.constraint_name == conflict_constraint
                                    && entry.row_offset == conflict_row_offset
                            })
                            .cloned()
                        else {
                            return Err(CopyInsertBatchError::from_storage_batch_error(err));
                        };

                        let Some(index) = schema
                            .indexes
                            .iter()
                            .find(|index| index.id == conflict_entry.index_id)
                        else {
                            return Err(CopyInsertBatchError::from_storage_batch_error(err));
                        };

                        if !index.unique
                            || !matches!(
                                index.state,
                                IndexState::Building | IndexState::WriteOnly | IndexState::Ready
                            )
                        {
                            return Err(CopyInsertBatchError::from_storage_batch_error(err));
                        }

                        let intra_batch_duplicate = pending_index_entries.iter().any(|entry| {
                            if entry.index_id != conflict_entry.index_id
                                || entry.idx_values != conflict_entry.idx_values
                                || entry.row_offset == conflict_entry.row_offset
                            {
                                return false;
                            }

                            pending_row_key_by_offset
                                .get(&entry.row_offset)
                                .is_some_and(|row_key| pending_row_keys.contains(row_key))
                        });
                        if intra_batch_duplicate {
                            // Intra-batch duplicates must surface direct 23505 and must not
                            // flow through stale-index reconciliation.
                            return Err(CopyInsertBatchError::from_storage_batch_error(err));
                        }

                        match resolve_unique_index_conflict(
                            &self.store,
                            txn,
                            db_id,
                            &schema,
                            index,
                            &conflict_entry.idx_values,
                            &conflict_entry.pk_values,
                        )
                        .await
                        .map_err(|e| CopyInsertBatchError::row(conflict_entry.row_offset, e))?
                        {
                            UniqueConflictResolution::Idempotent
                            | UniqueConflictResolution::StaleReplaced => {
                                pending_index_entries.retain(|entry| {
                                    !(entry.index_id == conflict_entry.index_id
                                        && entry.row_offset == conflict_entry.row_offset
                                        && entry.idx_values == conflict_entry.idx_values
                                        && entry.pk_values == conflict_entry.pk_values)
                                });
                            }
                            UniqueConflictResolution::RealConflict => {
                                return Err(CopyInsertBatchError::from_storage_batch_error(err));
                            }
                        }
                    }
                }
            }

            // ── Phase 4: GIN index entries (encode only, no duplicate check) ─
            for &(ref pk_values, row_offset) in &inserted {
                let (row, _) = &prepared_rows[row_offset];
                for index in &schema.indexes {
                    if matches!(index.state, IndexState::Building | IndexState::Invalid) {
                        continue;
                    }
                    let hashes =
                        crate::sql::gin::extract_gin_token_hashes_from_row(&schema, index, row)
                            .map_err(|e| CopyInsertBatchError::row(row_offset, e))?;
                    if hashes.is_empty() {
                        continue;
                    }
                    all_mutations.extend(self.store.encode_gin_index_mutations(
                        db_id,
                        schema.table_id,
                        index.id,
                        &hashes,
                        pk_values,
                    ));
                }
            }

            // ── Phase 5: single batch_mutate flush ─────────────────────
            // All data + B-tree index + GIN index mutations flushed in one
            // pessimistic lock RPC via batch_mutate.
            crate::txn::txn_batch_mutate(txn, all_mutations)
                .await
                .map_err(CopyInsertBatchError::non_row)?;

            // Propagate this batch's self-FK keys into the accumulated
            // cross-chunk state so subsequent CopyData frames can resolve
            // child-before-parent references to rows from earlier batches.
            if has_self_ref_fk {
                if let Some(ref mut acc) = accumulated_fk_keys {
                    for (fk_name, keys) in pending_ref_keys {
                        acc.entry(fk_name).or_default().extend(keys);
                    }
                }
            }

            Ok::<(), CopyInsertBatchError>(())
        }
        .await;

        if is_autocommit {
            if result.is_ok() {
                session
                    .commit()
                    .await
                    .map_err(CopyInsertBatchError::non_row)?;
            } else {
                session
                    .rollback()
                    .await
                    .map_err(CopyInsertBatchError::non_row)?;
            }
        }

        result
    }

    /// Validate deferred self-referencing FK constraints at CopyDone.
    /// All rows from every CopyData chunk are now in storage (within the
    /// transaction). Checks unresolved child FK references against the complete
    /// in-COPY PK key set.
    pub(crate) async fn validate_copy_deferred_self_fk(
        &self,
        session: &mut Session,
        table_name: &str,
        deferred_refs: &[(dml::ConstraintId, String, String)],
        pending_pk_keys: &HashMap<String, HashSet<String>>,
    ) -> Result<()> {
        let db_id = session.current_database_id();
        let (txn, _, _) = session
            .get_mut_txn_sequence_values_and_search_path()
            .ok_or_else(|| anyhow!("Transaction must be active"))?;
        let schema = self
            .store
            .get_schema(txn, db_id, table_name)
            .await?
            .ok_or_else(|| SqlError::RelationNotFound(table_name.to_string()))?;
        dml::validate_deferred_self_fk_refs(&schema, deferred_refs, pending_pk_keys)
    }

    /// Execute COPY FROM PARQUET with streaming insertion and transaction rotation.
    /// Called from the protocol handler after extension check is done.
    /// Handles: table resolution, parquet stream opening, column mapping, DML insertion.
    #[cfg(feature = "parquet")]
    pub async fn execute_copy_from_parquet(
        &self,
        session: &mut Session,
        table_name: &str,
        url: &str,
    ) -> Result<usize> {
        use crate::sql::dml;

        // Acquire per-tenant concurrency permit — held until function returns
        let tenant =
            crate::extensions::context::tenant_keyspace().unwrap_or_else(|| "default".to_string());
        let _permit = crate::extensions::parquet::limits::acquire_import_permit(&tenant)?;

        let db_id = session.current_database_id();
        let search_path: Vec<String> = session.search_path().to_vec();

        // Resolve table name via search_path.
        // Quoted identifiers preserve case; unquoted are lowercased (PostgreSQL rule).
        use crate::sql::names::normalize_ident_str;
        let (schema_opt, table_ident) = if table_name.contains('.') {
            let parts: Vec<&str> = table_name.splitn(2, '.').collect();
            if parts.len() == 2 {
                (
                    Some(normalize_ident_str(parts[0])),
                    normalize_ident_str(parts[1]),
                )
            } else {
                (None, normalize_ident_str(table_name))
            }
        } else {
            (None, normalize_ident_str(table_name))
        };

        // Open a batch stream from the Parquet file (row-group-at-a-time)
        let (_parquet_schema, batch_stream) =
            crate::extensions::parquet::reader::open_batch_stream(url).await?;

        let (txn, sequence_values, _sp) = session
            .get_mut_txn_sequence_values_and_search_path()
            .ok_or_else(|| anyhow!("Transaction must be active"))?;

        // Resolve table schema
        let (resolved_table, table_schema) = if let Some(ref schema_ident) = schema_opt {
            let resolved = format!("{}.{}", schema_ident, table_ident);
            let schema = self
                .store
                .get_schema(txn, db_id, &resolved)
                .await?
                .ok_or_else(|| anyhow!("relation \"{}\" does not exist", table_name))?;
            (resolved, schema)
        } else {
            let schemas: Vec<&str> = if search_path.is_empty() {
                vec!["public"]
            } else {
                search_path.iter().map(|s| s.as_str()).collect()
            };
            let mut found = None;
            for s in schemas {
                let resolved = format!("{}.{}", s, table_ident);
                if let Some(schema) = self.store.get_schema(txn, db_id, &resolved).await? {
                    found = Some((resolved, schema));
                    break;
                }
            }
            found.ok_or_else(|| anyhow!("relation \"{}\" does not exist", table_name))?
        };

        // Build column mapping: parquet column name (lowercase) -> table column index.
        let parquet_col_names: Vec<String> = _parquet_schema
            .columns
            .iter()
            .map(|c| c.name.to_lowercase())
            .collect();
        let mut parquet_to_table: Vec<Option<usize>> = Vec::with_capacity(parquet_col_names.len());
        for pq_name in &parquet_col_names {
            let idx = table_schema
                .columns
                .iter()
                .position(|c| c.name.eq_ignore_ascii_case(pq_name));
            if idx.is_none() {
                tracing::warn!(
                    "COPY FROM PARQUET: Parquet column '{}' not found in table '{}', skipping",
                    pq_name,
                    resolved_table
                );
            }
            parquet_to_table.push(idx);
        }

        // Build enum cache and compile check constraints ONCE
        let qctx = crate::sql::query_context::QueryContext::from_task_locals();
        let enum_cache =
            dml::build_enum_label_cache(&self.store, txn, db_id, &table_schema).await?;
        let write_plan = self.compile_write_row_plan(&table_schema, &qctx)?;

        // Stream batches and insert rows with transaction rotation
        use futures::StreamExt;
        futures::pin_mut!(batch_stream);
        let mut row_count: usize = 0;
        let mut batch_writes: usize = 0;
        let mut row_group_num: usize = 0;
        let commit_size: usize = 5000;
        let short_table = resolved_table.rsplit('.').next().unwrap_or(&resolved_table);

        while let Some(batch_result) = batch_stream.next().await {
            row_group_num += 1;
            let batch = batch_result
                .map_err(|e| anyhow!("COPY {}, row group {}: {}", short_table, row_group_num, e))?;
            for (row_in_group_idx, pq_values) in batch.into_iter().enumerate() {
                let row_in_group = row_in_group_idx + 1;
                // Map parquet values to table columns
                let mut row_values = vec![Value::Null; table_schema.columns.len()];
                let mut provided_indices: Vec<usize> = Vec::new();
                for (pq_idx, value) in pq_values.into_iter().enumerate() {
                    if let Some(Some(table_idx)) = parquet_to_table.get(pq_idx) {
                        row_values[*table_idx] = value;
                        provided_indices.push(*table_idx);
                    }
                }
                provided_indices.sort_unstable();
                provided_indices.dedup();
                self.reject_explicit_generated_insert_columns(&table_schema, &provided_indices)?;

                // Fill defaults for missing columns (serials, DEFAULT expressions)
                dml::fill_missing_columns(
                    &self.store,
                    txn,
                    db_id,
                    sequence_values,
                    &search_path,
                    &table_schema,
                    &mut row_values,
                    &provided_indices,
                )
                .await?;

                self.finalize_write_row(
                    txn,
                    db_id,
                    sequence_values,
                    &search_path,
                    &table_schema,
                    &write_plan,
                    &mut row_values,
                )
                .await
                .map_err(|e| {
                    anyhow!(
                        "COPY {}, row group {}, row {}: {}",
                        short_table,
                        row_group_num,
                        row_in_group,
                        e
                    )
                })?;

                let row = Row { values: row_values };

                // Insert the row (handles indexes, FK etc)
                let _ = dml::execute_insert_row(
                    &self.store,
                    txn,
                    db_id,
                    &resolved_table,
                    &table_schema,
                    row,
                    dml::ConflictBehavior::Error,
                    &enum_cache,
                    None,
                )
                .await
                .map_err(|e| {
                    anyhow!(
                        "COPY {}, row group {}, row {}: {}",
                        short_table,
                        row_group_num,
                        row_in_group,
                        e
                    )
                })?;

                row_count += 1;
                batch_writes += 1;

                // Transaction rotation every commit_size rows
                if batch_writes >= commit_size {
                    txn.commit().await?;
                    *txn = self.store.begin().await?;
                    batch_writes = 0;
                    tracing::info!(
                        "COPY FROM PARQUET: {} rows imported (committed) into {}",
                        row_count,
                        short_table
                    );
                }
            }
        }

        tracing::info!(
            "COPY FROM PARQUET: completed {} total rows into {}",
            row_count,
            short_table
        );
        Ok(row_count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sql::error::SqlError;

    #[test]
    fn from_storage_batch_error_extracts_row_offset() {
        let err: anyhow::Error = SqlError::UniqueViolation {
            constraint: "idx_email".to_string(),
            message: "duplicate key".to_string(),
            row_offset: Some(3),
        }
        .into();
        let batch_err = CopyInsertBatchError::from_storage_batch_error(err);
        assert_eq!(batch_err.failed_row_offset(), Some(3));
    }

    #[test]
    fn from_storage_batch_error_without_offset() {
        let err: anyhow::Error = SqlError::UniqueViolation {
            constraint: "idx_email".to_string(),
            message: "duplicate key".to_string(),
            row_offset: None,
        }
        .into();
        let batch_err = CopyInsertBatchError::from_storage_batch_error(err);
        assert_eq!(batch_err.failed_row_offset(), None);
    }

    #[test]
    fn from_storage_batch_error_non_unique_error() {
        let err = anyhow::anyhow!("some other error");
        let batch_err = CopyInsertBatchError::from_storage_batch_error(err);
        assert_eq!(batch_err.failed_row_offset(), None);
    }

    #[test]
    fn batch_error_row_preserves_offset() {
        let err = CopyInsertBatchError::row(7, anyhow::anyhow!("test"));
        assert_eq!(err.failed_row_offset(), Some(7));
    }

    #[test]
    fn batch_error_non_row_has_no_offset() {
        let err = CopyInsertBatchError::non_row(anyhow::anyhow!("test"));
        assert_eq!(err.failed_row_offset(), None);
    }
}
