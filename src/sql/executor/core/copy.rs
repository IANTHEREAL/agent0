//! COPY-related helpers

use super::*;

impl Executor {
    pub fn parse_value_for_copy(&self, val: &str, data_type: &DataType) -> Result<Value> {
        parse_value_for_copy(val, data_type)
    }

    pub async fn execute_copy_insert(
        &self,
        session: &mut Session,
        table_name: &str,
        col_values: Vec<(String, Value)>,
    ) -> Result<()> {
        let is_autocommit = !session.is_in_transaction();

        if is_autocommit {
            session.begin().await?;
        }

        let result = async {
            let db_id = session.current_database_id();
            let (txn, sequence_values, search_path) = session
                .get_mut_txn_sequence_values_and_search_path()
                .ok_or_else(|| anyhow!("Transaction must be active"))?;
            let schema = self
                .store
                .get_schema(txn, db_id, table_name)
                .await?
                .ok_or_else(|| SqlError::RelationNotFound(table_name.to_string()))?;
            let qctx = crate::sql::query_context::QueryContext::from_task_locals();
            let compiled_checks =
                crate::sql::check_constraints::compile_check_constraints(&schema, &qctx)?;

            let enum_cache = dml::build_enum_label_cache(&self.store, txn, db_id, &schema).await?;

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
            .await?;
            dml::coerce_row_values(&schema, &mut row_values)?;
            let row = Row { values: row_values };
            crate::sql::check_constraints::validate_compiled_check_constraints(
                &schema,
                &compiled_checks,
                &row,
                &qctx,
            )?;

            let _ = dml::execute_insert_row(
                &self.store,
                txn,
                db_id,
                table_name,
                &schema,
                row,
                dml::ConflictBehavior::Error,
                &enum_cache,
            )
            .await?;

            Ok::<(), anyhow::Error>(())
        }
        .await;

        if is_autocommit {
            if result.is_ok() {
                session.commit().await?;
            } else {
                session.rollback().await?;
            }
        }

        result
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
        let normalize_ident = |s: &str| -> String {
            let trimmed = s.trim();
            if trimmed.starts_with('"') && trimmed.ends_with('"') && trimmed.len() > 1 {
                trimmed[1..trimmed.len() - 1].to_string()
            } else {
                trimmed.to_lowercase()
            }
        };
        let (schema_opt, table_ident) = if table_name.contains('.') {
            let parts: Vec<&str> = table_name.splitn(2, '.').collect();
            if parts.len() == 2 {
                (Some(normalize_ident(parts[0])), normalize_ident(parts[1]))
            } else {
                (None, normalize_ident(table_name))
            }
        } else {
            (None, normalize_ident(table_name))
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
        let compiled_checks =
            crate::sql::check_constraints::compile_check_constraints(&table_schema, &qctx)?;

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
            let mut row_in_group: usize = 0;

            for pq_values in batch {
                row_in_group += 1;
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

                // Coerce types
                dml::coerce_row_values(&table_schema, &mut row_values).map_err(|e| {
                    anyhow!(
                        "COPY {}, row group {}, row {}: {}",
                        short_table,
                        row_group_num,
                        row_in_group,
                        e
                    )
                })?;

                let row = Row { values: row_values };

                // Validate check constraints
                crate::sql::check_constraints::validate_compiled_check_constraints(
                    &table_schema,
                    &compiled_checks,
                    &row,
                    &qctx,
                )
                .map_err(|e| {
                    anyhow!(
                        "COPY {}, row group {}, row {}: {}",
                        short_table,
                        row_group_num,
                        row_in_group,
                        e
                    )
                })?;

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
