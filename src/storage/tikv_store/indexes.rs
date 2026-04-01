use super::*;
use crate::sql::error::SqlError;
use crate::storage::backpressure::tikv_op;
use crate::txn::configured_key_size_limit;

const INDEX_SENTINEL_VALUE: &[u8] = &[0x01];

/// Check an encoded index key against TiKV's `max-key-size` limit and return
/// an actionable `IndexKeyTooLarge` error when it exceeds the threshold.
fn check_index_key_size(idx_key: &[u8]) -> Result<()> {
    let limit = configured_key_size_limit();
    if limit == 0 || idx_key.len() < limit {
        return Ok(());
    }
    Err(SqlError::IndexKeyTooLarge {
        message: format!(
            "index row requires {} bytes, exceeds maximum {} bytes\n\
             HINT: Values larger than {} bytes cannot be indexed with btree. \
             Consider an expression index on a prefix (e.g. CREATE INDEX ON t (left(col, 200))), \
             a GIN index for full-text search, or removing the btree index on this column.",
            idx_key.len(),
            limit,
            limit,
        ),
    }
    .into())
}

/// A single index entry to be created as part of a batch insert.
#[derive(Debug, Clone)]
pub(crate) struct BatchIndexEntry {
    pub index_id: u64,
    pub idx_values: Vec<Value>,
    pub pk_values: Vec<Value>,
    pub unique: bool,
    pub row_offset: usize,
    pub constraint_name: String,
    pub key_columns: Vec<String>,
}

impl TikvStore {
    fn build_batch_unique_violation(entry: &BatchIndexEntry) -> SqlError {
        let vals: Vec<String> = entry.idx_values.iter().map(|v| format!("{}", v)).collect();
        SqlError::UniqueViolation {
            constraint: entry.constraint_name.clone(),
            message: format!(
                "duplicate key value violates unique constraint \"{}\"\nDETAIL:  Key ({})=({}) already exists.",
                entry.constraint_name,
                entry.key_columns.join(", "),
                vals.join(", ")
            ),
            row_offset: Some(entry.row_offset),
        }
    }

    #[inline]
    fn index_key_has_null(values: &[Value]) -> bool {
        values.iter().any(|v| matches!(v, Value::Null))
    }

    /// Encode an index key into the fully-prefixed KV key.
    pub fn make_index_key(
        &self,
        db_id: u64,
        table_id: u64,
        index_id: u64,
        values: &[Value],
        pk_values: Option<&[Value]>,
    ) -> Vec<u8> {
        self.key(&encode_index_key_v2(
            db_id, table_id, index_id, values, pk_values,
        ))
    }

    /// Decode primary-key values from a non-unique index key.
    pub fn decode_non_unique_pk_from_index_key(
        &self,
        full_key: &[u8],
        db_id: u64,
        table_id: u64,
        index_id: u64,
        index_column_types: &[DataType],
        pk_types: &[DataType],
    ) -> Result<Vec<Value>> {
        let fixed_prefix_len = self
            .make_index_key(db_id, table_id, index_id, &[], None)
            .len();
        self.decode_non_unique_pk_inner(
            full_key,
            fixed_prefix_len,
            index_column_types,
            pk_types,
            db_id,
            table_id,
            index_id,
        )
    }

    /// Decode primary-key values from a non-unique index key given a
    /// pre-computed prefix length.
    ///
    /// Policy: fails fast on any malformed or truncated key, consistent with
    /// `scan_index`'s behaviour for unique indexes. Corruption is surfaced
    /// immediately rather than silently skipped.
    fn decode_non_unique_pk_inner(
        &self,
        full_key: &[u8],
        fixed_prefix_len: usize,
        index_column_types: &[DataType],
        pk_types: &[DataType],
        db_id: u64,
        table_id: u64,
        index_id: u64,
    ) -> Result<Vec<Value>> {
        if full_key.len() <= fixed_prefix_len {
            return Err(anyhow!(
                "non-unique index key too short (db_id={}, table_id={}, index_id={})",
                db_id,
                table_id,
                index_id
            ));
        }
        let mut offset = fixed_prefix_len;
        for data_type in index_column_types {
            let (_, consumed) = decode_value_memcomparable(&full_key[offset..], data_type)?;
            offset += consumed;
        }
        if full_key.get(offset) != Some(&0x01) {
            return Err(anyhow!(
                "non-unique index key missing PK separator byte (db_id={}, table_id={}, index_id={})",
                db_id, table_id, index_id
            ));
        }
        offset += 1;
        decode_pk_from_index_suffix(&full_key[offset..], pk_types)
    }

    /// Decode primary-key values from a unique index entry.
    ///
    /// For unique indexes that allow NULL (stored with PK-suffixed key shape), the value uses
    /// a sentinel and the PK lives in the key suffix. Legacy entries may still have empty values.
    /// For normal unique entries, the PK is encoded in the value.
    pub(crate) fn decode_unique_pk_from_index_entry(
        &self,
        full_key: &[u8],
        value: &[u8],
        db_id: u64,
        table_id: u64,
        index_id: u64,
        index_column_types: &[DataType],
        pk_types: &[DataType],
    ) -> Result<Vec<Value>> {
        if value.is_empty() || value == INDEX_SENTINEL_VALUE {
            self.decode_non_unique_pk_from_index_key(
                full_key,
                db_id,
                table_id,
                index_id,
                index_column_types,
                pk_types,
            )
        } else {
            decode_pk_from_index_suffix(value, pk_types)
        }
    }

    /// Create a single index entry.  Returns the number of bytes written
    /// (key + value) so callers can track transaction byte budgets.
    pub async fn create_index_entry(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        table_id: u64,
        index_id: u64,
        values: &[Value],
        pk_values: &[Value],
        unique: bool,
    ) -> Result<usize> {
        let enforce_unique_lookup = unique && !Self::index_key_has_null(values);
        if enforce_unique_lookup {
            let idx_key = self.key(&encode_index_key_v2(
                db_id, table_id, index_id, values, None,
            ));
            check_index_key_size(&idx_key)?;
            if tikv_op!(txn.get(idx_key.clone()).await)?.is_some() {
                return Err(crate::storage::unique_index_duplicate_error());
            }
            let idx_val = encode_pk_values(pk_values);
            let bytes_written = idx_key.len() + idx_val.len();
            txn_put(txn, idx_key, idx_val).await?;
            Ok(bytes_written)
        } else {
            let idx_key = self.key(&encode_index_key_v2(
                db_id,
                table_id,
                index_id,
                values,
                Some(pk_values),
            ));
            check_index_key_size(&idx_key)?;
            let bytes_written = idx_key.len() + INDEX_SENTINEL_VALUE.len();
            txn_put(txn, idx_key, INDEX_SENTINEL_VALUE.to_vec()).await?;
            Ok(bytes_written)
        }
    }

    /// Batch-create index entries, using a single `batch_get` for the unique
    /// duplicate check instead of one `get` per entry.
    ///
    /// On unique violation returns an error; the caller can use `row_offset`
    /// from the returned error context to attribute the failure.
    ///
    /// `old_keys_being_deleted`: when called from batch UPDATE, the set of
    /// old index keys being deleted in the same batch.  A unique key that
    /// exists in TiKV but is also in this set is not a real conflict (it
    /// will be freed by the same batch flush).  Pass `None` for INSERT/COPY.
    ///
    /// Returns the encoded `(key, value)` mutations — caller is responsible
    /// for flushing via `txn_batch_mutate`.
    pub async fn create_index_entries_batch(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        table_id: u64,
        entries: &[BatchIndexEntry],
        old_keys_being_deleted: Option<&HashSet<Vec<u8>>>,
    ) -> Result<Vec<(Vec<u8>, Vec<u8>)>> {
        if entries.is_empty() {
            return Ok(Vec::new());
        }

        // Separate entries that need a unique-check GET from those that don't.
        struct UniqueEntry {
            idx_key: Vec<u8>,
            idx_val: Vec<u8>,
            entry: BatchIndexEntry,
        }
        struct NonUniqueEntry {
            idx_key: Vec<u8>,
        }

        let mut unique_entries: Vec<UniqueEntry> = Vec::new();
        let mut non_unique_entries: Vec<NonUniqueEntry> = Vec::new();

        for entry in entries {
            let enforce_unique = entry.unique && !Self::index_key_has_null(&entry.idx_values);
            if enforce_unique {
                let idx_key = self.key(&encode_index_key_v2(
                    db_id,
                    table_id,
                    entry.index_id,
                    &entry.idx_values,
                    None,
                ));
                check_index_key_size(&idx_key)?;
                let idx_val = encode_pk_values(&entry.pk_values);
                unique_entries.push(UniqueEntry {
                    idx_key,
                    idx_val,
                    entry: entry.clone(),
                });
            } else {
                let idx_key = self.key(&encode_index_key_v2(
                    db_id,
                    table_id,
                    entry.index_id,
                    &entry.idx_values,
                    Some(&entry.pk_values),
                ));
                check_index_key_size(&idx_key)?;
                non_unique_entries.push(NonUniqueEntry { idx_key });
            }
        }

        // Phase 2: prefetch existing unique index keys from TiKV.
        if !unique_entries.is_empty() {
            let mut keys: Vec<Vec<u8>> = Vec::new();
            let mut deduped: HashSet<Vec<u8>> = HashSet::with_capacity(unique_entries.len());
            for ue in &unique_entries {
                if deduped.insert(ue.idx_key.clone()) {
                    keys.push(ue.idx_key.clone());
                }
            }
            drop(deduped);

            let mut existing_map: HashMap<Vec<u8>, Vec<u8>> = HashMap::new();
            for chunk in keys.chunks(BATCH_GET_CHUNK_SIZE) {
                kv_stats::record_batch_get_keys(chunk.len());
                for pair in txn
                    .batch_get(chunk.iter().cloned())
                    .await?
                    .collect::<Vec<tikv_client::KvPair>>()
                {
                    let k: Vec<u8> = pair.0.into();
                    let v: Vec<u8> = pair.1;
                    existing_map.insert(k, v);
                }
            }

            // Phase 3: row-order-preserving conflict check.
            // Sort unique entries by row_offset to process in insertion order.
            // For each entry, check both storage conflicts (from prefetched map)
            // and intra-batch duplicates (from previously-seen keys). The first
            // entry to fail either check is the reported conflict — matching
            // PostgreSQL's row-by-row semantics.
            unique_entries.sort_by_key(|ue| ue.entry.row_offset);
            let mut seen_keys: HashSet<Vec<u8>> = HashSet::with_capacity(unique_entries.len());
            for ue in &unique_entries {
                if let Some(existing_val) = existing_map.get(&ue.idx_key) {
                    // Idempotent writes (same index key already points to the same PK)
                    // are not conflicts and should not fail COPY retry paths.
                    if existing_val.as_slice() != ue.idx_val.as_slice() {
                        // For batch UPDATE: if the conflicting key is being
                        // deleted in the same batch (e.g. two rows swapping
                        // unique values), this is not a real conflict.
                        let being_deleted =
                            old_keys_being_deleted.is_some_and(|s| s.contains(&ue.idx_key));
                        if !being_deleted {
                            return Err(Self::build_batch_unique_violation(&ue.entry).into());
                        }
                    }
                }
                if !seen_keys.insert(ue.idx_key.clone()) {
                    return Err(Self::build_batch_unique_violation(&ue.entry).into());
                }
            }
        }

        // Collect mutations (caller flushes via txn_batch_mutate).
        let mut mutations = Vec::with_capacity(unique_entries.len() + non_unique_entries.len());
        for ue in unique_entries {
            mutations.push((ue.idx_key, ue.idx_val));
        }
        for ne in non_unique_entries {
            mutations.push((ne.idx_key, INDEX_SENTINEL_VALUE.to_vec()));
        }
        Ok(mutations)
    }

    /// Encode an index deletion key without performing any IO.
    /// Mirrors the key construction logic of [`delete_index_entry`].
    pub fn encode_index_deletion_key(
        &self,
        db_id: u64,
        table_id: u64,
        index_id: u64,
        values: &[Value],
        pk_values: &[Value],
        unique: bool,
    ) -> Vec<u8> {
        let enforce_unique = unique && !Self::index_key_has_null(values);
        if enforce_unique {
            self.key(&encode_index_key_v2(
                db_id, table_id, index_id, values, None,
            ))
        } else {
            self.key(&encode_index_key_v2(
                db_id,
                table_id,
                index_id,
                values,
                Some(pk_values),
            ))
        }
    }

    /// Encode GIN index deletion keys without performing any IO.
    pub fn encode_gin_index_deletion_keys(
        &self,
        db_id: u64,
        table_id: u64,
        index_id: u64,
        token_hashes: &[u64],
        pk_values: &[Value],
    ) -> Vec<Vec<u8>> {
        let pk_key = encode_pk_values(pk_values);
        token_hashes
            .iter()
            .map(|&th| {
                self.key(&encode_gin_index_key_v2(
                    db_id, table_id, index_id, th, &pk_key,
                ))
            })
            .collect()
    }

    /// Delete an index entry
    pub async fn delete_index_entry(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        table_id: u64,
        index_id: u64,
        values: &[Value],
        pk_values: &[Value],
        unique: bool,
    ) -> Result<()> {
        let enforce_unique_lookup = unique && !Self::index_key_has_null(values);
        if enforce_unique_lookup {
            let idx_key = self.key(&encode_index_key_v2(
                db_id, table_id, index_id, values, None,
            ));
            txn_delete(txn, idx_key).await?;
        } else {
            let idx_key = self.key(&encode_index_key_v2(
                db_id,
                table_id,
                index_id,
                values,
                Some(pk_values),
            ));
            txn_delete(txn, idx_key).await?;
        }
        Ok(())
    }

    /// Scan index to get PKs
    pub async fn scan_index(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        table_id: u64,
        index_id: u64,
        values: &[Value],
        unique: bool,
        pk_types: &[DataType],
        limit: Option<usize>,
    ) -> Result<Vec<Vec<Value>>> {
        if pk_types.is_empty() {
            return Err(anyhow!("PK types required for index scan"));
        }

        if matches!(limit, Some(0)) {
            return Ok(Vec::new());
        }

        let enforce_unique_lookup = unique && !Self::index_key_has_null(values);
        if enforce_unique_lookup {
            let idx_key = self.key(&encode_index_key_v2(
                db_id, table_id, index_id, values, None,
            ));
            if let Some(val) = tikv_op!(txn.get(idx_key).await)? {
                let pk = decode_pk_from_index_suffix(&val, pk_types)?;
                Ok(vec![pk])
            } else {
                Ok(vec![])
            }
        } else {
            let prefix = encode_index_key_v2(db_id, table_id, index_id, values, None);

            let mut start_raw = prefix.clone();
            start_raw.push(0x01);
            let start_key = self.key(&start_raw);

            let mut end_raw = prefix;
            end_raw.push(0x02);
            let end_key = self.key(&end_raw);

            let range: BoundRange = (start_key.clone()..end_key).into();
            let pairs = tikv_op!(txn.scan(range, scan_limit_to_u32(limit)).await)
                .map_err(|e| anyhow!(e))?;

            let mut pks = Vec::new();
            let mut scanned_pairs = 0usize;
            for pair in pairs {
                scanned_pairs += 1;
                let full_key: &[u8] = pair.key().as_ref().into();
                if full_key.len() <= start_key.len() {
                    continue;
                }
                let pk_bytes = &full_key[start_key.len()..];
                let pk = decode_pk_from_index_suffix(pk_bytes, pk_types)?;
                pks.push(pk);
            }
            kv_stats::record_index_scan_pairs(scanned_pairs);
            Ok(pks)
        }
    }

    /// Scan index by a prefix of the index values to get PKs.
    ///
    /// This is used for composite indexes when only the leading columns are constrained.
    /// For unique indexes, the PK is stored in the value. For non-unique indexes, the PK is
    /// stored as a suffix in the key after all index values and a separator byte.
    pub async fn scan_index_prefix(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        table_id: u64,
        index_id: u64,
        prefix_values: &[Value],
        unique: bool,
        index_column_types: &[DataType],
        pk_types: &[DataType],
        limit: Option<usize>,
    ) -> Result<Vec<Vec<Value>>> {
        if pk_types.is_empty() {
            return Err(anyhow!("PK types required for index scan"));
        }

        if matches!(limit, Some(0)) {
            return Ok(Vec::new());
        }

        let prefix = encode_index_key_v2(db_id, table_id, index_id, prefix_values, None);
        let start_key = self.key(&prefix);

        let mut end_raw = prefix;
        end_raw.push(0xFF);
        let end_key = self.key(&end_raw);

        let range: BoundRange = (start_key..end_key).into();
        let pairs =
            tikv_op!(txn.scan(range, scan_limit_to_u32(limit)).await).map_err(|e| anyhow!(e))?;

        let mut pks = Vec::new();
        if unique {
            let mut scanned_pairs = 0usize;
            for pair in pairs {
                scanned_pairs += 1;
                let full_key: &[u8] = pair.key().as_ref().into();
                let value: &[u8] = pair.value().as_ref();
                let pk = self.decode_unique_pk_from_index_entry(
                    full_key,
                    value,
                    db_id,
                    table_id,
                    index_id,
                    index_column_types,
                    pk_types,
                )?;
                pks.push(pk);
            }
            kv_stats::record_index_scan_pairs(scanned_pairs);
            return Ok(pks);
        }

        // Compute once per scan (not per row) to avoid the per-row allocation cost of
        // make_index_key inside decode_non_unique_pk_from_index_key.
        let fixed_prefix_len = self
            .make_index_key(db_id, table_id, index_id, &[], None)
            .len();
        let mut scanned_pairs = 0usize;
        for pair in pairs {
            scanned_pairs += 1;
            let full_key: &[u8] = pair.key().as_ref().into();
            let pk = self.decode_non_unique_pk_inner(
                full_key,
                fixed_prefix_len,
                index_column_types,
                pk_types,
                db_id,
                table_id,
                index_id,
            )?;
            pks.push(pk);
        }

        kv_stats::record_index_scan_pairs(scanned_pairs);
        Ok(pks)
    }

    /// Scan index by bounded range on the next index column after `prefix_values`.
    pub async fn scan_index_range(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        table_id: u64,
        index_id: u64,
        prefix_values: &[Value],
        range_start: Option<&Value>,
        start_inclusive: bool,
        range_end: Option<&Value>,
        end_inclusive: bool,
        unique: bool,
        index_column_types: &[DataType],
        pk_types: &[DataType],
        limit: Option<usize>,
    ) -> Result<Vec<Vec<Value>>> {
        if pk_types.is_empty() {
            return Err(anyhow!("PK types required for index scan"));
        }

        if matches!(limit, Some(0)) {
            return Ok(Vec::new());
        }

        let start_raw = encode_index_range_start_v2(
            db_id,
            table_id,
            index_id,
            prefix_values,
            range_start,
            start_inclusive,
        );
        let end_raw = encode_index_range_end_v2(
            db_id,
            table_id,
            index_id,
            prefix_values,
            range_end,
            end_inclusive,
        );
        let start_key = self.key(&start_raw);
        let end_key = self.key(&end_raw);

        let range: BoundRange = (start_key..end_key).into();
        let pairs =
            tikv_op!(txn.scan(range, scan_limit_to_u32(limit)).await).map_err(|e| anyhow!(e))?;

        let mut pks = Vec::new();
        if unique {
            let mut scanned_pairs = 0usize;
            for pair in pairs {
                scanned_pairs += 1;
                let full_key: &[u8] = pair.key().as_ref().into();
                let value: &[u8] = pair.value().as_ref();
                let pk = self.decode_unique_pk_from_index_entry(
                    full_key,
                    value,
                    db_id,
                    table_id,
                    index_id,
                    index_column_types,
                    pk_types,
                )?;
                pks.push(pk);
            }
            kv_stats::record_index_scan_pairs(scanned_pairs);
            return Ok(pks);
        }

        // Compute once per scan (not per row) to avoid the per-row allocation cost of
        // make_index_key inside decode_non_unique_pk_from_index_key.
        let fixed_prefix_len = self
            .make_index_key(db_id, table_id, index_id, &[], None)
            .len();
        let mut scanned_pairs = 0usize;
        for pair in pairs {
            scanned_pairs += 1;
            let full_key: &[u8] = pair.key().as_ref().into();
            let pk = self.decode_non_unique_pk_inner(
                full_key,
                fixed_prefix_len,
                index_column_types,
                pk_types,
                db_id,
                table_id,
                index_id,
            )?;
            pks.push(pk);
        }

        kv_stats::record_index_scan_pairs(scanned_pairs);
        Ok(pks)
    }

    /// Create GIN-like inverted index entries for a row.
    ///
    /// Each `token_hash` is stored as a separate key that points to `pk_values` via the
    /// key suffix. The value uses a non-empty sentinel.
    pub async fn create_gin_index_entries(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        table_id: u64,
        index_id: u64,
        token_hashes: &[u64],
        pk_values: &[Value],
    ) -> Result<()> {
        if token_hashes.is_empty() {
            return Ok(());
        }

        let pk_key = encode_pk_values(pk_values);
        for &token_hash in token_hashes {
            let key = self.key(&encode_gin_index_key_v2(
                db_id, table_id, index_id, token_hash, &pk_key,
            ));
            txn_put(txn, key, INDEX_SENTINEL_VALUE.to_vec()).await?;
        }
        Ok(())
    }

    /// Encode GIN index entry mutations without writing to TiKV.
    ///
    /// Returns `(key, value)` pairs for batch collection; caller flushes
    /// via `txn_batch_mutate`.
    pub fn encode_gin_index_mutations(
        &self,
        db_id: u64,
        table_id: u64,
        index_id: u64,
        token_hashes: &[u64],
        pk_values: &[Value],
    ) -> Vec<(Vec<u8>, Vec<u8>)> {
        if token_hashes.is_empty() {
            return Vec::new();
        }
        let pk_key = encode_pk_values(pk_values);
        token_hashes
            .iter()
            .map(|&token_hash| {
                let key = self.key(&encode_gin_index_key_v2(
                    db_id, table_id, index_id, token_hash, &pk_key,
                ));
                (key, INDEX_SENTINEL_VALUE.to_vec())
            })
            .collect()
    }

    /// Delete GIN-like inverted index entries for a row.
    pub async fn delete_gin_index_entries(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        table_id: u64,
        index_id: u64,
        token_hashes: &[u64],
        pk_values: &[Value],
    ) -> Result<()> {
        if token_hashes.is_empty() {
            return Ok(());
        }

        let pk_key = encode_pk_values(pk_values);
        for &token_hash in token_hashes {
            let key = self.key(&encode_gin_index_key_v2(
                db_id, table_id, index_id, token_hash, &pk_key,
            ));
            txn_delete(txn, key).await?;
        }
        Ok(())
    }

    /// Batch get rows by PKs
    pub async fn batch_get_rows(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        table_id: u64,
        pks: Vec<Vec<Value>>,
        _schema: &TableSchema,
    ) -> Result<Vec<Row>> {
        let mut rows = Vec::with_capacity(pks.len());
        let mut data_keys: Vec<Vec<u8>> = Vec::with_capacity(BATCH_GET_CHUNK_SIZE);

        for pk in &pks {
            let row_key = encode_pk_values(pk);
            data_keys.push(self.key(&encode_data_key_v2(db_id, table_id, &row_key)));

            if data_keys.len() >= BATCH_GET_CHUNK_SIZE {
                self.batch_get_rows_by_data_keys(txn, &data_keys, &mut rows)
                    .await?;
                data_keys.clear();
            }
        }

        if !data_keys.is_empty() {
            self.batch_get_rows_by_data_keys(txn, &data_keys, &mut rows)
                .await?;
        }

        Ok(rows)
    }

    async fn batch_get_rows_by_data_keys(
        &self,
        txn: &mut Transaction,
        data_keys: &[Vec<u8>],
        out: &mut Vec<Row>,
    ) -> Result<()> {
        kv_stats::record_batch_get_keys(data_keys.len());
        let pairs =
            tikv_op!(txn.batch_get(data_keys.iter().cloned()).await).map_err(|e| anyhow!(e))?;
        let mut by_key: HashMap<Key, tikv_client::Value> = HashMap::with_capacity(data_keys.len());

        for pair in pairs {
            let tikv_client::KvPair(key, value) = pair;
            by_key.insert(key, value);
        }

        for key in data_keys {
            let key_ref: &Key = key.into();
            if let Some(val) = by_key.get(key_ref) {
                out.push(deserialize_row(val)?);
            }
        }

        Ok(())
    }

    /// Scan a single GIN posting list for the given token hash.
    ///
    /// Returns a `Vec<Vec<u8>>` of raw PK byte sequences (un-decoded).
    /// The caller is responsible for decoding these with
    /// `decode_pk_from_index_suffix`.  Keeping them as raw bytes allows
    /// efficient set operations (intersect / union) before decoding.
    pub async fn scan_gin_posting_list_page(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        table_id: u64,
        index_id: u64,
        token_hash: u64,
        cursor: Option<&[u8]>,
        page_size: u32,
    ) -> Result<(Vec<Vec<u8>>, Option<Vec<u8>>)> {
        let prefix = self.key(&encode_gin_index_prefix_v2(
            db_id, table_id, index_id, token_hash,
        ));
        let prefix_len = prefix.len();

        let end_key = {
            let mut end = prefix.clone();
            if let Some(last) = end.last_mut() {
                *last = last.wrapping_add(1);
            }
            end
        };

        let start_key = match cursor {
            Some(existing_cursor) => existing_cursor.to_vec(),
            None => prefix.clone(),
        };
        let range: BoundRange = (start_key..end_key).into();
        let pairs = tikv_op!(txn.scan(range, page_size).await)?;

        let mut pk_bytes_list = Vec::new();
        let mut last_key: Option<Vec<u8>> = None;
        let mut scanned = 0usize;
        for pair in pairs {
            scanned += 1;
            let full_key: &[u8] = pair.key().as_ref().into();
            last_key = Some(full_key.to_vec());
            if full_key.len() > prefix_len {
                pk_bytes_list.push(full_key[prefix_len..].to_vec());
            }
        }
        kv_stats::record_index_scan_pairs(scanned);

        let next_cursor = if (pk_bytes_list.len() as u32) < page_size {
            None
        } else {
            last_key.map(|mut key| {
                key.push(0x00);
                key
            })
        };

        Ok((pk_bytes_list, next_cursor))
    }

    /// Estimate the size of a GIN posting list by scanning up to `probe_limit`
    /// entries. Returns `(count, saturated)` where `saturated=true` means the
    /// true posting-list size is at least `count`.
    pub async fn probe_gin_posting_list_size(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        table_id: u64,
        index_id: u64,
        token_hash: u64,
        probe_limit: u32,
    ) -> Result<(u32, bool)> {
        let (pk_bytes_list, next_cursor) = self
            .scan_gin_posting_list_page(
                txn,
                db_id,
                table_id,
                index_id,
                token_hash,
                None,
                probe_limit,
            )
            .await?;
        Ok((pk_bytes_list.len() as u32, next_cursor.is_some()))
    }

    /// Filter `pk_bytes_list` to only PKs that exist in the posting list for
    /// `token_hash`, preserving the input order.
    pub async fn filter_gin_posting_membership(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        table_id: u64,
        index_id: u64,
        token_hash: u64,
        pk_bytes_list: &[Vec<u8>],
    ) -> Result<Vec<Vec<u8>>> {
        if pk_bytes_list.is_empty() {
            return Ok(Vec::new());
        }

        let keys: Vec<Vec<u8>> = pk_bytes_list
            .iter()
            .map(|pk_bytes| {
                self.key(&encode_gin_index_key_v2(
                    db_id, table_id, index_id, token_hash, pk_bytes,
                ))
            })
            .collect();
        let mut existing: HashSet<Vec<u8>> = HashSet::with_capacity(keys.len());
        for chunk in keys.chunks(BATCH_GET_CHUNK_SIZE) {
            kv_stats::record_batch_get_keys(chunk.len());
            for pair in
                tikv_op!(txn.batch_get(chunk.iter().cloned()).await).map_err(|e| anyhow!(e))?
            {
                let tikv_client::KvPair(key, _) = pair;
                let key_vec: Vec<u8> = key.into();
                existing.insert(key_vec);
            }
        }

        Ok(keys
            .into_iter()
            .zip(pk_bytes_list.iter().cloned())
            .filter_map(|(full_key, pk_bytes)| existing.contains(&full_key).then_some(pk_bytes))
            .collect())
    }

    /// Scan table data keys only and return raw PK byte sequences without
    /// materializing full rows.
    pub async fn scan_table_primary_key_bytes_page(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        table_id: u64,
        cursor: Option<&[u8]>,
        page_size: u32,
    ) -> Result<(Vec<Vec<u8>>, Option<Vec<u8>>)> {
        let (table_start, table_end) = encode_table_data_range_v2(db_id, table_id);
        let prefix_len = table_start.len();
        let start_key = cursor.map_or(table_start.clone(), |cursor| cursor.to_vec());
        let range: BoundRange = (start_key..table_end).into();
        let pairs = tikv_op!(txn.scan(range, page_size).await)?;

        let mut pk_bytes_list = Vec::new();
        let mut last_key: Option<Vec<u8>> = None;
        let mut scanned = 0usize;
        for pair in pairs {
            scanned += 1;
            let full_key: &[u8] = pair.key().as_ref().into();
            last_key = Some(full_key.to_vec());
            if full_key.len() > prefix_len {
                pk_bytes_list.push(full_key[prefix_len..].to_vec());
            }
        }
        kv_stats::record_table_scan_pairs(scanned);

        let next_cursor = if (pk_bytes_list.len() as u32) < page_size {
            None
        } else {
            last_key.map(|mut key| {
                key.push(0x00);
                key
            })
        };

        Ok((pk_bytes_list, next_cursor))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_non_unique_pk_from_index_key_roundtrip_single_pk() {
        let store = TikvStore::new_stub();
        let idx_values = vec![Value::Int32(42), Value::Text("abc".to_string())];
        let pk_values = vec![Value::Int64(7)];
        let key = store.make_index_key(1, 2, 3, &idx_values, Some(pk_values.as_slice()));

        let decoded = store
            .decode_non_unique_pk_from_index_key(
                &key,
                1,
                2,
                3,
                &[DataType::Int32, DataType::Text],
                &[DataType::Int64],
            )
            .expect("decode non-unique index PK");
        assert_eq!(decoded, pk_values);
    }

    #[test]
    fn decode_non_unique_pk_from_index_key_roundtrip_composite_pk() {
        let store = TikvStore::new_stub();
        let idx_values = vec![Value::Text("v".to_string())];
        let pk_values = vec![Value::Int32(11), Value::Uuid([0; 16])];
        let key = store.make_index_key(9, 8, 7, &idx_values, Some(pk_values.as_slice()));

        let decoded = store
            .decode_non_unique_pk_from_index_key(
                &key,
                9,
                8,
                7,
                &[DataType::Text],
                &[DataType::Int32, DataType::Uuid],
            )
            .expect("decode composite PK");
        assert_eq!(decoded, pk_values);
    }

    #[test]
    fn decode_non_unique_pk_from_index_key_rejects_unique_key_shape() {
        let store = TikvStore::new_stub();
        let key = store.make_index_key(1, 2, 3, &[Value::Int32(1)], None);

        let err = store
            .decode_non_unique_pk_from_index_key(
                &key,
                1,
                2,
                3,
                &[DataType::Int32],
                &[DataType::Int32],
            )
            .expect_err("unique key shape should be rejected");
        assert!(err.to_string().contains("missing PK separator"));
    }

    #[test]
    fn decode_unique_pk_from_index_entry_accepts_sentinel_suffix_encoding() {
        let store = TikvStore::new_stub();
        let idx_values = vec![Value::Text("idx".to_string())];
        let pk_values = vec![Value::Int64(99)];
        let full_key = store.make_index_key(1, 2, 3, &idx_values, Some(pk_values.as_slice()));

        let decoded = store
            .decode_unique_pk_from_index_entry(
                &full_key,
                INDEX_SENTINEL_VALUE,
                1,
                2,
                3,
                &[DataType::Text],
                &[DataType::Int64],
            )
            .expect("decode sentinel-backed unique entry");

        assert_eq!(decoded, pk_values);
    }

    #[test]
    fn decode_unique_pk_from_index_entry_accepts_legacy_empty_suffix_encoding() {
        let store = TikvStore::new_stub();
        let idx_values = vec![Value::Int32(17)];
        let pk_values = vec![Value::Int32(5), Value::Text("pk".to_string())];
        let full_key = store.make_index_key(7, 8, 9, &idx_values, Some(pk_values.as_slice()));

        let decoded = store
            .decode_unique_pk_from_index_entry(
                &full_key,
                &[],
                7,
                8,
                9,
                &[DataType::Int32],
                &[DataType::Int32, DataType::Text],
            )
            .expect("decode legacy empty-value unique entry");

        assert_eq!(decoded, pk_values);
    }
}
