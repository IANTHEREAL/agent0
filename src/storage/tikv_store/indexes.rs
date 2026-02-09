use super::*;

impl TikvStore {
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
        if unique {
            let idx_key = self.key(&encode_index_key_v2(
                db_id, table_id, index_id, values, None,
            ));
            if txn.get(idx_key.clone()).await?.is_some() {
                return Err(anyhow!("Duplicate entry for unique index"));
            }
            let idx_val = encode_pk_values(pk_values);
            txn_put(txn, idx_key, idx_val).await?;
        } else {
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
        if unique {
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

        if unique {
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
                let pk_bytes: &[u8] = pair.value().as_ref();
                let pk = decode_pk_from_index_suffix(pk_bytes, pk_types)?;
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
                let pk_bytes: &[u8] = pair.value().as_ref();
                let pk = decode_pk_from_index_suffix(pk_bytes, pk_types)?;
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

    /// Scan a GIN-like inverted index for rows matching **all** token hashes.
    ///
    /// This returns encoded PK keys (the same bytes used in `t_{table_id}_{pk}` keys)
    /// to allow callers to fetch rows without decoding/re-encoding PK values.
    pub async fn scan_gin_index_intersection(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        table_id: u64,
        index_id: u64,
        token_hashes: &[u64],
    ) -> Result<Vec<Vec<u8>>> {
        if token_hashes.is_empty() {
            return Ok(Vec::new());
        }

        // Probe posting sizes to scan the smallest posting list first.
        //
        // This avoids materializing a high-cardinality token into `candidates` (memory spikes),
        // and increases the chance we can early-exit before scanning large postings when the
        // intersection becomes empty.
        const GIN_POSTING_PROBE_LIMIT: u32 = 4096;

        struct TokenProbe {
            orig_pos: usize,
            token_hash: u64,
            estimated_postings: usize,
            cached_pk_keys: Option<Vec<Vec<u8>>>,
        }

        fn pk_suffix<'a>(full_key: &'a [u8], prefix_len: usize) -> Option<&'a [u8]> {
            if full_key.len() <= prefix_len {
                None
            } else {
                Some(&full_key[prefix_len..])
            }
        }

        let probe_scan_limit = GIN_POSTING_PROBE_LIMIT.saturating_add(1);
        let mut probes = Vec::with_capacity(token_hashes.len());
        for (orig_pos, &token_hash) in token_hashes.iter().enumerate() {
            let (start_raw, end_raw) =
                encode_gin_index_token_range_v2(db_id, table_id, index_id, token_hash);
            let start_key = self.key(&start_raw);
            let end_key = self.key(&end_raw);
            let prefix_len = start_key.len();

            let range: BoundRange = (start_key.clone()..end_key).into();
            let keys: Vec<_> = txn.scan_keys(range, probe_scan_limit).await?.collect();
            kv_stats::record_gin_scan_keys(keys.len());
            let estimated_postings = keys.len();

            let cached_pk_keys = if estimated_postings < probe_scan_limit as usize {
                let mut pk_keys = Vec::with_capacity(estimated_postings);
                for key in keys {
                    let full_key: &[u8] = key.as_ref().into();
                    let Some(pk_bytes) = pk_suffix(full_key, prefix_len) else {
                        continue;
                    };
                    pk_keys.push(pk_bytes.to_vec());
                }
                Some(pk_keys)
            } else {
                None
            };

            probes.push(TokenProbe {
                orig_pos,
                token_hash,
                estimated_postings,
                cached_pk_keys,
            });
        }

        probes.sort_by_key(|p| (p.estimated_postings, p.orig_pos));

        let mut candidates: HashSet<Vec<u8>> = HashSet::with_capacity(
            probes
                .first()
                .map(|p| p.estimated_postings)
                .unwrap_or_default(),
        );

        for (i, probe) in probes.into_iter().enumerate() {
            if i > 0 && candidates.is_empty() {
                break;
            }

            if let Some(pk_keys) = probe.cached_pk_keys {
                if i == 0 {
                    candidates.extend(pk_keys);
                } else {
                    let mut next: HashSet<Vec<u8>> = HashSet::with_capacity(candidates.len());
                    for pk_key in pk_keys {
                        if candidates.contains(&pk_key) {
                            next.insert(pk_key);
                        }
                    }
                    candidates = next;
                }
                continue;
            }

            let (start_raw, end_raw) =
                encode_gin_index_token_range_v2(db_id, table_id, index_id, probe.token_hash);
            let start_key = self.key(&start_raw);
            let end_key = self.key(&end_raw);
            let prefix_len = start_key.len();

            if i == 0 {
                let range: BoundRange = (start_key..end_key).into();
                let keys = txn.scan_keys(range, SCAN_LIMIT).await?;
                let mut scanned_keys = 0usize;
                for key in keys {
                    scanned_keys += 1;
                    let full_key: &[u8] = key.as_ref().into();
                    let Some(pk_bytes) = pk_suffix(full_key, prefix_len) else {
                        continue;
                    };
                    candidates.insert(pk_bytes.to_vec());
                }
                kv_stats::record_gin_scan_keys(scanned_keys);
            } else {
                let mut next: HashSet<Vec<u8>> = HashSet::with_capacity(candidates.len());
                let range: BoundRange = (start_key..end_key).into();
                let keys = txn.scan_keys(range, SCAN_LIMIT).await?;
                let mut scanned_keys = 0usize;
                for key in keys {
                    scanned_keys += 1;
                    let full_key: &[u8] = key.as_ref().into();
                    let Some(pk_bytes) = pk_suffix(full_key, prefix_len) else {
                        continue;
                    };
                    if candidates.contains(pk_bytes) {
                        next.insert(pk_bytes.to_vec());
                    }
                }
                kv_stats::record_gin_scan_keys(scanned_keys);
                candidates = next;
            }
        }

        let mut out: Vec<Vec<u8>> = candidates.into_iter().collect();
        out.sort();
        Ok(out)
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

    /// Batch get rows by their encoded PK keys.
    ///
    /// `pk_keys` are the raw bytes produced by `encode_pk_values` for the table's PK.
    pub async fn batch_get_rows_by_pk_keys(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        table_id: u64,
        pk_keys: Vec<Vec<u8>>,
    ) -> Result<Vec<Row>> {
        let mut rows = Vec::with_capacity(pk_keys.len());
        let mut data_keys: Vec<Vec<u8>> = Vec::with_capacity(BATCH_GET_CHUNK_SIZE);

        for pk_key in &pk_keys {
            data_keys.push(self.key(&encode_data_key_v2(db_id, table_id, pk_key)));

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
