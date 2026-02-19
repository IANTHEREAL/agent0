use super::*;

impl TikvStore {
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
        if full_key.len() <= fixed_prefix_len {
            return Err(anyhow!("Non-unique index key too short"));
        }

        let mut offset = fixed_prefix_len;
        for data_type in index_column_types {
            let (_, consumed) = decode_value_memcomparable(&full_key[offset..], data_type)?;
            offset += consumed;
        }

        if full_key.get(offset) != Some(&0x01) {
            return Err(anyhow!("Non-unique index key missing PK separator"));
        }
        offset += 1;

        let pk_bytes = &full_key[offset..];
        decode_pk_from_index_suffix(pk_bytes, pk_types)
    }

    pub async fn create_index_entry(
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
            if txn.get(idx_key.clone()).await?.is_some() {
                return Err(crate::storage::unique_index_duplicate_error());
            }
            let idx_val = encode_pk_values(pk_values);
            txn_put(txn, idx_key, idx_val).await?;
        } else {
            // PostgreSQL unique indexes treat NULL values as distinct by default.
            // For unique keys containing NULL, persist with PK-suffixed key shape
            // (same as non-unique) so multiple NULL rows can coexist.
            let idx_key = self.key(&encode_index_key_v2(
                db_id,
                table_id,
                index_id,
                values,
                Some(pk_values),
            ));
            txn_put(txn, idx_key, vec![]).await?;
        }
        Ok(())
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
            if let Some(val) = txn.get(idx_key).await? {
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
            let pairs = txn.scan(range, scan_limit_to_u32(limit)).await?;

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
        let pairs = txn.scan(range, scan_limit_to_u32(limit)).await?;

        let mut pks = Vec::new();
        if unique {
            let mut scanned_pairs = 0usize;
            for pair in pairs {
                scanned_pairs += 1;
                let pk = if pair.value().is_empty() {
                    let full_key: &[u8] = pair.key().as_ref().into();
                    self.decode_non_unique_pk_from_index_key(
                        full_key,
                        db_id,
                        table_id,
                        index_id,
                        index_column_types,
                        pk_types,
                    )?
                } else {
                    let pk_bytes: &[u8] = pair.value().as_ref();
                    decode_pk_from_index_suffix(pk_bytes, pk_types)?
                };
                pks.push(pk);
            }
            kv_stats::record_index_scan_pairs(scanned_pairs);
            return Ok(pks);
        }

        let fixed_prefix_len = encode_index_key_v2(db_id, table_id, index_id, &[], None).len();
        let mut scanned_pairs = 0usize;
        for pair in pairs {
            scanned_pairs += 1;
            let full_key: &[u8] = pair.key().as_ref().into();
            if full_key.len() <= fixed_prefix_len {
                continue;
            }

            let mut offset = fixed_prefix_len;
            for data_type in index_column_types {
                let (_, consumed) = decode_value_memcomparable(&full_key[offset..], data_type)?;
                offset += consumed;
            }

            if full_key.get(offset) != Some(&0x01) {
                return Err(anyhow!("Non-unique index key missing PK separator"));
            }
            offset += 1;

            let pk_bytes = &full_key[offset..];
            let pk = decode_pk_from_index_suffix(pk_bytes, pk_types)?;
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
        let pairs = txn.scan(range, scan_limit_to_u32(limit)).await?;

        let mut pks = Vec::new();
        if unique {
            let mut scanned_pairs = 0usize;
            for pair in pairs {
                scanned_pairs += 1;
                let pk = if pair.value().is_empty() {
                    let full_key: &[u8] = pair.key().as_ref().into();
                    self.decode_non_unique_pk_from_index_key(
                        full_key,
                        db_id,
                        table_id,
                        index_id,
                        index_column_types,
                        pk_types,
                    )?
                } else {
                    let pk_bytes: &[u8] = pair.value().as_ref();
                    decode_pk_from_index_suffix(pk_bytes, pk_types)?
                };
                pks.push(pk);
            }
            kv_stats::record_index_scan_pairs(scanned_pairs);
            return Ok(pks);
        }

        let fixed_prefix_len = encode_index_key_v2(db_id, table_id, index_id, &[], None).len();
        let mut scanned_pairs = 0usize;
        for pair in pairs {
            scanned_pairs += 1;
            let full_key: &[u8] = pair.key().as_ref().into();
            if full_key.len() <= fixed_prefix_len {
                continue;
            }

            let mut offset = fixed_prefix_len;
            for data_type in index_column_types {
                let (_, consumed) = decode_value_memcomparable(&full_key[offset..], data_type)?;
                offset += consumed;
            }

            if full_key.get(offset) != Some(&0x01) {
                return Err(anyhow!("Non-unique index key missing PK separator"));
            }
            offset += 1;

            let pk_bytes = &full_key[offset..];
            let pk = decode_pk_from_index_suffix(pk_bytes, pk_types)?;
            pks.push(pk);
        }

        kv_stats::record_index_scan_pairs(scanned_pairs);
        Ok(pks)
    }

    /// Create GIN-like inverted index entries for a row.
    ///
    /// Each `token_hash` is stored as a separate key that points to `pk_values` via the
    /// key suffix. The value is empty.
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
            txn_put(txn, key, Vec::new()).await?;
        }
        Ok(())
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
        let pairs = txn.batch_get(data_keys.iter().cloned()).await?;
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
}
