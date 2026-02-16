use super::*;
use crate::sql::error::SqlError;

impl TikvStore {
    /// Acquire pessimistic (exclusive) locks on the given rows.
    ///
    /// Used for both FOR UPDATE and FOR SHARE.  TiKV only supports exclusive
    /// pessimistic locks — there is no shared row-level lock — so FOR SHARE
    /// is effectively upgraded to FOR UPDATE.
    pub async fn lock_rows(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        table_name: &str,
        rows: &[Row],
    ) -> Result<()> {
        let schema = self
            .get_schema(txn, db_id, table_name)
            .await?
            .ok_or_else(|| anyhow!("Table not found"))?;
        if schema.pk_indices.is_empty() {
            return Err(anyhow!(
                "cannot lock rows: table '{}' has no primary key",
                table_name
            ));
        }
        let keys: Vec<Vec<u8>> = rows
            .iter()
            .map(|row| {
                let pk_values = schema.get_pk_values(row);
                let row_key = encode_pk_values(&pk_values);
                self.key(&encode_data_key_v2(db_id, schema.table_id, &row_key))
            })
            .collect();
        txn.lock_keys(keys).await.map_err(|e| anyhow!(e))
    }

    /// Lock rows with SKIP LOCKED semantics.  Returns the indices (into `rows`)
    /// that were successfully locked in `txn`.
    ///
    /// Attempts a NOWAIT lock on each row directly in the caller's transaction.
    /// Rows held by another transaction are skipped; rows already held by this
    /// transaction succeed without error.  Non-lock errors (network, region,
    /// etc.) are propagated immediately.
    pub async fn lock_rows_skip_locked(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        table_name: &str,
        rows: &[Row],
        max_locks: Option<usize>,
    ) -> Result<Vec<usize>> {
        if rows.is_empty() {
            return Ok(Vec::new());
        }

        let schema = self
            .get_schema(txn, db_id, table_name)
            .await?
            .ok_or_else(|| anyhow!("Table not found"))?;
        if schema.pk_indices.is_empty() {
            return Err(anyhow!(
                "cannot lock rows: table '{}' has no primary key",
                table_name
            ));
        }

        let max_locks = max_locks.unwrap_or(usize::MAX);

        let keys: Vec<Vec<u8>> = rows
            .iter()
            .map(|row| {
                let pk_values = schema.get_pk_values(row);
                let row_key = encode_pk_values(&pk_values);
                self.key(&encode_data_key_v2(db_id, schema.table_id, &row_key))
            })
            .collect();

        // Try-lock each key with NOWAIT directly in the caller's txn.
        // - Keys already held by this txn succeed (no self-lock false positive).
        // - Keys held by another txn fail immediately (no blocking).
        // - Non-lock errors are propagated, not silently swallowed.
        let mut available_indices = Vec::new();
        for (idx, key) in keys.into_iter().enumerate() {
            if available_indices.len() >= max_locks {
                break;
            }
            match txn.lock_keys_nowait(vec![key]).await {
                Ok(()) => available_indices.push(idx),
                Err(e) if e.is_lock_conflict() => continue,
                Err(e) => return Err(anyhow!(e)),
            }
        }

        Ok(available_indices)
    }

    /// Lock rows with NOWAIT semantics.  Fails immediately with
    /// `SqlError::LockNotAvailable` (SQLSTATE 55P03) if any row is locked by
    /// another transaction.
    ///
    /// Locks are acquired directly in the caller's transaction, so rows
    /// already held by this transaction succeed without error (matching
    /// PostgreSQL semantics).  Non-lock errors are propagated as-is.
    pub async fn lock_rows_nowait(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        table_name: &str,
        rows: &[Row],
    ) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }

        let schema = self
            .get_schema(txn, db_id, table_name)
            .await?
            .ok_or_else(|| anyhow!("Table not found"))?;
        if schema.pk_indices.is_empty() {
            return Err(anyhow!(
                "cannot lock rows: table '{}' has no primary key",
                table_name
            ));
        }

        let keys: Vec<Vec<u8>> = rows
            .iter()
            .map(|row| {
                let pk_values = schema.get_pk_values(row);
                let row_key = encode_pk_values(&pk_values);
                self.key(&encode_data_key_v2(db_id, schema.table_id, &row_key))
            })
            .collect();

        // Lock directly in the caller's txn with NOWAIT semantics.
        // - Self-locks succeed (same TiKV txn recognises its own locks).
        // - Lock conflicts return an error immediately (wait_timeout = -1).
        // - Non-lock errors propagate without being misclassified as 55P03.
        match txn.lock_keys_nowait(keys).await {
            Ok(()) => Ok(()),
            Err(e) if e.is_lock_conflict() => Err(crate::sql::error::SqlError::LockNotAvailable {
                relation: table_name.to_string(),
            }
            .into()),
            Err(e) => Err(anyhow!(e)),
        }
    }

    /// Reserve a relation name in the schema-wide namespace.
    ///
    /// Writes a `sys_relname_{schema}.{name}` key with a single-byte tag.
    /// If the key already exists the name is taken — returns
    /// `Err(SqlError::DuplicateRelation)`.
    pub async fn reserve_relation_name(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        full_name: &str,
    ) -> Result<()> {
        let key = self.key(&encode_relname_key_v2(db_id, full_name));
        if txn.get(key.clone()).await?.is_some() {
            let short_name = full_name.rsplit('.').next().unwrap_or(full_name);
            return Err(SqlError::DuplicateRelation(short_name.to_string()).into());
        }
        txn_put(txn, key, vec![b'I']).await?;
        Ok(())
    }

    /// Release a previously reserved relation name (no-op if missing).
    pub async fn release_relation_name(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        full_name: &str,
    ) -> Result<()> {
        let key = self.key(&encode_relname_key_v2(db_id, full_name));
        txn_delete(txn, key).await?;
        Ok(())
    }

    /// Check if a table exists (using txn)
    pub async fn table_exists(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        table_name: &str,
    ) -> Result<bool> {
        let key = self.key(&encode_schema_key_v2(db_id, table_name));
        let exists = txn.get(key).await?.is_some();
        Ok(exists)
    }

    pub async fn create_table(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        schema: TableSchema,
    ) -> Result<()> {
        let schema_key = self.key(&encode_schema_key_v2(db_id, &schema.name));
        if txn.get(schema_key.clone()).await?.is_some() {
            let short_name = schema.name.rsplit('.').next().unwrap_or(&schema.name);
            return Err(anyhow!("relation \"{}\" already exists", short_name));
        }
        let schema_data = serialize_schema(&schema)?;
        txn_put(txn, schema_key, schema_data).await?;
        info!(
            "Created table '{}' with ID {}",
            schema.name, schema.table_id
        );
        Ok(())
    }

    /// Get a table schema by name
    pub async fn get_schema(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        table_name: &str,
    ) -> Result<Option<TableSchema>> {
        let key = self.key(&encode_schema_key_v2(db_id, table_name));
        let val = txn.get(key).await?;
        match val {
            Some(data) => Ok(Some(deserialize_schema(&data)?)),
            None => Ok(None),
        }
    }

    pub async fn drop_table(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        table_name: &str,
    ) -> Result<bool> {
        let schema_opt = self.get_schema(txn, db_id, table_name).await?;
        if let Some(schema) = schema_opt {
            let schema_key = self.key(&encode_schema_key_v2(db_id, table_name));
            txn_delete(txn, schema_key).await?;
            let (raw_start, raw_end) = encode_table_data_range_v2(db_id, schema.table_id);
            let range: BoundRange = (self.key(&raw_start)..self.key(&raw_end)).into();
            let pairs = txn.scan(range, SCAN_LIMIT).await?;
            for pair in pairs {
                txn_delete(txn, pair.into_key().into()).await?;
            }

            let (raw_start, raw_end) = encode_table_index_range_v2(db_id, schema.table_id);
            let range: BoundRange = (self.key(&raw_start)..self.key(&raw_end)).into();
            let pairs = txn.scan(range, SCAN_LIMIT).await?;
            for pair in pairs {
                txn_delete(txn, pair.into_key().into()).await?;
            }

            // Remove the per-table sequence counter (used by TableId-backed sequences),
            // but only if no sequence definition still references this table_id.
            //
            // `ALTER SEQUENCE ... OWNED BY NONE` preserves the sequence while leaving it
            // backed by the historical per-table `_sys_seq_ + table_id` key.
            let table_id = schema.table_id;
            let has_table_id_sequence =
                self.list_sequences(txn, db_id).await?.iter().any(
                    |def| matches!(&def.backing, SequenceBacking::TableId(id) if *id == table_id),
                );
            if !has_table_id_sequence {
                let seq_key = self.key(&encode_table_sequence_value_key_v2(db_id, table_id));
                txn_delete(txn, seq_key).await?;
            }

            // Release relation-name reservation keys for indexes and PK
            // so the names become available for reuse.
            let schema_name = table_name.splitn(2, '.').next().unwrap_or("public");
            for idx in &schema.indexes {
                let idx_full = format!("{}.{}", schema_name, idx.name);
                self.release_relation_name(txn, db_id, &idx_full).await?;
            }
            if let Some(pk_name) = &schema.pk_constraint_name {
                if !pk_name.is_empty() {
                    let pk_full = format!("{}.{}", schema_name, pk_name);
                    self.release_relation_name(txn, db_id, &pk_full).await?;
                }
            }

            // Comments are stored under name-keyed keys; ensure they do not resurrect after
            // DROP + recreate with the same name.
            txn_delete(
                txn,
                self.key(&encode_comment_table_key_v2(db_id, table_name)),
            )
            .await?;
            self.delete_column_comments_for_table(txn, db_id, table_name)
                .await?;

            // Remove persisted ANALYZE statistics for this table.
            self.delete_statistics(txn, db_id, table_id).await?;

            info!("Dropped table '{}'", table_name);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Insert a row into a table
    pub async fn insert(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        table_name: &str,
        row: Row,
    ) -> Result<Vec<Value>> {
        let schema = self
            .get_schema(txn, db_id, table_name)
            .await?
            .ok_or_else(|| anyhow!("Table not found"))?;
        if row.values.len() != schema.columns.len() {
            return Err(anyhow!("Column count mismatch"));
        }
        let pk_values = schema.get_pk_values(&row);
        let row_key = encode_pk_values(&pk_values);
        let data_key = self.key(&encode_data_key_v2(db_id, schema.table_id, &row_key));
        let row_data = serialize_row(&row)?;
        if txn.get(data_key.clone()).await?.is_some() {
            let short_table = table_name.rsplit('.').next().unwrap_or(table_name);
            let constraint_name = schema
                .pk_constraint_name
                .clone()
                .unwrap_or_else(|| format!("{}_pkey", short_table));
            let pk_col_names: Vec<String> = schema
                .pk_indices
                .iter()
                .map(|&i| schema.columns[i].name.clone())
                .collect();
            let pk_val_strs: Vec<String> = pk_values
                .iter()
                .map(|v| match v {
                    crate::types::Value::Int32(n) => n.to_string(),
                    crate::types::Value::Int64(n) => n.to_string(),
                    crate::types::Value::Text(s) => s.clone(),
                    crate::types::Value::Uuid(bytes) => uuid::Uuid::from_bytes(*bytes).to_string(),
                    other => format!("{}", other),
                })
                .collect();
            let message = format!(
                "duplicate key value violates unique constraint \"{}\"\nDETAIL:  Key ({})=({}) already exists.",
                constraint_name,
                pk_col_names.join(", "),
                pk_val_strs.join(", ")
            );
            return Err(SqlError::UniqueViolation {
                constraint: constraint_name,
                message,
            }
            .into());
        }
        txn_put(txn, data_key, row_data).await?;
        debug!("Inserted row into '{}'", table_name);
        Ok(pk_values)
    }

    /// Upsert a row into a table
    pub async fn upsert(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        table_name: &str,
        row: Row,
    ) -> Result<()> {
        let schema = self
            .get_schema(txn, db_id, table_name)
            .await?
            .ok_or_else(|| anyhow!("Table not found"))?;
        let pk_values = schema.get_pk_values(&row);
        let row_key = encode_pk_values(&pk_values);
        let data_key = self.key(&encode_data_key_v2(db_id, schema.table_id, &row_key));
        let row_data = serialize_row(&row)?;
        txn_put(txn, data_key, row_data).await?;
        debug!("Upserted row into '{}'", table_name);
        Ok(())
    }

    /// Upsert a row into a table with an explicit physical PK value.
    ///
    /// This is required for tables without an explicit primary key, where the
    /// physical row key is not derivable from row values.
    pub async fn upsert_by_pk(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        table_name: &str,
        pk_values: &[Value],
        row: Row,
    ) -> Result<()> {
        let schema = self
            .get_schema(txn, db_id, table_name)
            .await?
            .ok_or_else(|| anyhow!("Table not found"))?;
        if row.values.len() != schema.columns.len() {
            return Err(anyhow!("Column count mismatch"));
        }
        let row_key = encode_pk_values(pk_values);
        let data_key = self.key(&encode_data_key_v2(db_id, schema.table_id, &row_key));
        let row_data = serialize_row(&row)?;
        txn_put(txn, data_key, row_data).await?;
        debug!("Upserted row into '{}' (explicit PK)", table_name);
        Ok(())
    }

    /// Scan all rows from a table
    pub async fn scan(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        table_name: &str,
        limit: Option<usize>,
    ) -> Result<Vec<Row>> {
        let schema = self
            .get_schema(txn, db_id, table_name)
            .await?
            .ok_or_else(|| anyhow!("Table not found"))?;
        let (raw_start, raw_end) = encode_table_data_range_v2(db_id, schema.table_id);
        let range: BoundRange = (self.key(&raw_start)..self.key(&raw_end)).into();
        let pairs = txn.scan(range, scan_limit_to_u32(limit)).await?;
        let mut rows = Vec::new();
        let mut scanned_pairs = 0usize;
        for pair in pairs {
            scanned_pairs += 1;
            let row = deserialize_row(pair.value())?;
            rows.push(row);
        }
        kv_stats::record_table_scan_pairs(scanned_pairs);
        debug!("Scanned {} rows from '{}'", rows.len(), table_name);
        Ok(rows)
    }

    /// Delete rows matching a simple condition
    pub async fn delete_by_pk(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        table_name: &str,
        pk_values: &[Value],
    ) -> Result<u64> {
        let schema = self
            .get_schema(txn, db_id, table_name)
            .await?
            .ok_or_else(|| anyhow!("Table not found"))?;
        let row_key = encode_pk_values(pk_values);
        let data_key = self.key(&encode_data_key_v2(db_id, schema.table_id, &row_key));
        let existed = txn.get(data_key.clone()).await?.is_some();
        if existed {
            txn_delete(txn, data_key).await?;
            Ok(1)
        } else {
            Ok(0)
        }
    }

    pub async fn list_tables(&self, txn: &mut Transaction, db_id: u64) -> Result<Vec<String>> {
        let prefix = encode_schema_prefix_v2(db_id);
        let mut end = prefix.clone();
        end.push(0xFF);
        let range: BoundRange = (prefix.clone()..end).into();
        let pairs = txn.scan(range, SCAN_LIMIT).await?;
        let mut tables = Vec::new();
        for pair in pairs {
            let key: &[u8] = pair.key().as_ref().into();
            if key.starts_with(&prefix) {
                let name = String::from_utf8_lossy(&key[prefix.len()..]).to_string();
                tables.push(name);
            }
        }

        Ok(tables)
    }

    /// Truncate a table
    pub async fn truncate_table(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        table_name: &str,
    ) -> Result<bool> {
        let schema_opt = self.get_schema(txn, db_id, table_name).await?;
        if let Some(schema) = schema_opt {
            let (raw_start, raw_end) = encode_table_data_range_v2(db_id, schema.table_id);
            let range: BoundRange = (self.key(&raw_start)..self.key(&raw_end)).into();
            let pairs = txn.scan(range, SCAN_LIMIT).await?;
            for pair in pairs {
                txn_delete(txn, pair.into_key().into()).await?;
            }

            let (raw_start, raw_end) = encode_table_index_range_v2(db_id, schema.table_id);
            let range: BoundRange = (self.key(&raw_start)..self.key(&raw_end)).into();
            let pairs = txn.scan(range, SCAN_LIMIT).await?;
            for pair in pairs {
                txn_delete(txn, pair.into_key().into()).await?;
            }
            info!("Truncated table '{}'", table_name);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Update table schema
    pub async fn update_schema(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        schema: TableSchema,
    ) -> Result<()> {
        let schema_key = self.key(&encode_schema_key_v2(db_id, &schema.name));
        let schema_data = serialize_schema(&schema)?;
        txn_put(txn, schema_key, schema_data).await?;
        Ok(())
    }

    /// Rename a table by moving its schema metadata key.
    ///
    /// This operation does **not** rewrite row or index keys because those are
    /// keyed by `table_id`, not by table name.
    ///
    /// Callers are responsible for updating cross-table references (e.g.
    /// `foreign_keys[*].ref_table`) if needed.
    pub async fn rename_table_schema(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        old_table: &str,
        new_table: &str,
    ) -> Result<()> {
        if old_table == new_table {
            return Ok(());
        }

        let old_key = self.key(&encode_schema_key_v2(db_id, old_table));
        let new_key = self.key(&encode_schema_key_v2(db_id, new_table));

        if txn.get(new_key.clone()).await?.is_some() {
            return Err(anyhow!("Table '{}' already exists", new_table));
        }

        let schema_bytes = txn
            .get(old_key.clone())
            .await?
            .ok_or_else(|| anyhow!("Table '{}' does not exist", old_table))?;

        let mut schema = deserialize_schema(&schema_bytes)?;
        schema.name = new_table.to_string();
        schema.version = schema
            .version
            .checked_add(1)
            .ok_or_else(|| anyhow!("Schema version overflow"))?;

        txn_put(txn, new_key, serialize_schema(&schema)?).await?;
        txn_delete(txn, old_key).await?;

        Ok(())
    }

    /// Rewrite name-keyed metadata for a renamed table.
    ///
    /// This updates (moves) trigger keys, table/column comment keys, and rewrites
    /// `SequenceDef.owned_by` string references that point at the renamed table.
    pub async fn rename_table_metadata(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        old_table: &str,
        new_table: &str,
    ) -> Result<()> {
        if old_table == new_table {
            return Ok(());
        }

        self.rename_table_triggers(txn, db_id, old_table, new_table)
            .await?;
        self.rename_table_comments(txn, db_id, old_table, new_table)
            .await?;
        self.rewrite_sequences_owned_by_table(txn, db_id, old_table, new_table)
            .await?;
        Ok(())
    }

    /// Rewrite name-keyed metadata for a renamed column.
    ///
    /// This updates column comment keys and rewrites `SequenceDef.owned_by` column
    /// references for sequences owned by the renamed column.
    pub async fn rename_column_metadata(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        table_full_name: &str,
        old_column: &str,
        new_column: &str,
    ) -> Result<()> {
        if old_column == new_column {
            return Ok(());
        }

        self.rename_column_comment(txn, db_id, table_full_name, old_column, new_column)
            .await?;
        self.rewrite_sequences_owned_by_column(txn, db_id, table_full_name, old_column, new_column)
            .await?;
        Ok(())
    }
}
