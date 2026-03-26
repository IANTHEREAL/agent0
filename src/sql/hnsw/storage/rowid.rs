use super::*;

pub fn hnsw_rid_pk2rid_key(db_id: u64, table_id: u64, pk_bytes: &[u8]) -> Vec<u8> {
    let mut key = format!("d_{db_id}_hnsw_rid_pk2rid_{table_id}_").into_bytes();
    key.extend_from_slice(pk_bytes);
    key
}

pub fn hnsw_rid_rid2pk_key(db_id: u64, table_id: u64, rowid: u64) -> Vec<u8> {
    let mut key = format!("d_{db_id}_hnsw_rid_rid2pk_{table_id}_").into_bytes();
    key.extend_from_slice(&rowid.to_be_bytes());
    key
}

pub fn hnsw_rid_seq_key(db_id: u64, table_id: u64) -> Vec<u8> {
    format!("d_{db_id}_hnsw_rid_seq_{table_id}").into_bytes()
}

pub fn hnsw_rid_rid2pk_prefix(db_id: u64, table_id: u64) -> Vec<u8> {
    format!("d_{db_id}_hnsw_rid_rid2pk_{table_id}_").into_bytes()
}

pub fn hnsw_rid_pk2rid_prefix(db_id: u64, table_id: u64) -> Vec<u8> {
    format!("d_{db_id}_hnsw_rid_pk2rid_{table_id}_").into_bytes()
}

pub async fn get_rowid_for_pk(
    txn: &mut Transaction,
    db_id: u64,
    table_id: u64,
    pk_bytes: &[u8],
) -> Result<Option<u64>, SqlError> {
    let key = hnsw_rid_pk2rid_key(db_id, table_id, pk_bytes);
    match txn
        .get(key)
        .await
        .map_err(|e| SqlError::Internal(anyhow::anyhow!(e)))?
    {
        Some(val) => {
            let arr: [u8; 8] = val
                .try_into()
                .map_err(|_| SqlError::Internal(anyhow::anyhow!("corrupt pk2rid value")))?;
            Ok(Some(u64::from_be_bytes(arr)))
        }
        None => Ok(None),
    }
}

pub async fn put_rowid_mapping(
    txn: &mut Transaction,
    db_id: u64,
    table_id: u64,
    pk_bytes: &[u8],
    rowid: u64,
) -> Result<(), SqlError> {
    let pk2rid_key = hnsw_rid_pk2rid_key(db_id, table_id, pk_bytes);
    let rid2pk_key = hnsw_rid_rid2pk_key(db_id, table_id, rowid);
    txn_put(txn, pk2rid_key, rowid.to_be_bytes().to_vec())
        .await
        .map_err(|e| SqlError::Internal(anyhow::anyhow!(e)))?;
    txn_put(txn, rid2pk_key, pk_bytes.to_vec())
        .await
        .map_err(|e| SqlError::Internal(anyhow::anyhow!(e)))?;
    Ok(())
}

pub async fn batch_get_pk_for_rowids(
    txn: &mut Transaction,
    db_id: u64,
    table_id: u64,
    rowids: &[u64],
) -> Result<Vec<Option<Vec<u8>>>, SqlError> {
    if rowids.is_empty() {
        return Ok(Vec::new());
    }
    let keys: Vec<Vec<u8>> = rowids
        .iter()
        .map(|&rid| hnsw_rid_rid2pk_key(db_id, table_id, rid))
        .collect();
    let pairs: Vec<tikv_client::KvPair> = txn
        .batch_get(keys.clone())
        .await
        .map_err(|e| SqlError::Internal(anyhow::anyhow!(e)))?
        .collect();
    let mut map = std::collections::HashMap::with_capacity(pairs.len());
    for pair in &pairs {
        let k: &[u8] = pair.key().as_ref().into();
        map.insert(k.to_vec(), pair.value().to_vec());
    }
    let result = keys.into_iter().map(|k| map.remove(&k)).collect();
    Ok(result)
}

pub async fn delete_rowid_mapping(
    txn: &mut Transaction,
    db_id: u64,
    table_id: u64,
    pk_bytes: &[u8],
    rowid: u64,
) -> Result<(), SqlError> {
    let pk2rid_key = hnsw_rid_pk2rid_key(db_id, table_id, pk_bytes);
    let rid2pk_key = hnsw_rid_rid2pk_key(db_id, table_id, rowid);
    txn_delete(txn, pk2rid_key)
        .await
        .map_err(|e| SqlError::Internal(anyhow::anyhow!(e)))?;
    txn_delete(txn, rid2pk_key)
        .await
        .map_err(|e| SqlError::Internal(anyhow::anyhow!(e)))?;
    Ok(())
}

pub async fn reassign_rowid_mapping(
    txn: &mut Transaction,
    db_id: u64,
    table_id: u64,
    old_pk_bytes: &[u8],
    new_pk_bytes: &[u8],
) -> Result<Option<u64>, SqlError> {
    let Some(rowid) = get_rowid_for_pk(txn, db_id, table_id, old_pk_bytes).await? else {
        return Ok(None);
    };
    delete_rowid_mapping(txn, db_id, table_id, old_pk_bytes, rowid).await?;
    put_rowid_mapping(txn, db_id, table_id, new_pk_bytes, rowid).await?;
    Ok(Some(rowid))
}

pub async fn delete_all_rowid_mappings(
    txn: &mut Transaction,
    db_id: u64,
    table_id: u64,
) -> Result<(), SqlError> {
    txn_delete(txn, hnsw_rid_seq_key(db_id, table_id))
        .await
        .map_err(|e| SqlError::Internal(anyhow::anyhow!(e)))?;
    delete_keys_by_prefix(txn, &hnsw_rid_pk2rid_prefix(db_id, table_id)).await?;
    delete_keys_by_prefix(txn, &hnsw_rid_rid2pk_prefix(db_id, table_id)).await?;
    Ok(())
}

async fn delete_keys_by_prefix(txn: &mut Transaction, prefix: &[u8]) -> Result<(), SqlError> {
    let mut end = prefix.to_vec();
    if let Some(last) = end.last_mut() {
        *last = last.checked_add(1).unwrap_or(0xFF);
    }
    let mut start = prefix.to_vec();
    loop {
        let range: BoundRange = (start.clone()..end.clone()).into();
        let pairs: Vec<tikv_client::KvPair> = txn
            .scan(range, DELTA_SCAN_BATCH_SIZE)
            .await
            .map_err(|e| SqlError::Internal(anyhow::anyhow!(e)))?
            .collect();
        let count = pairs.len();
        let mut last_key: Option<Vec<u8>> = None;
        for pair in pairs {
            let k: &[u8] = pair.key().as_ref().into();
            let key: Vec<u8> = k.to_vec();
            if !key.starts_with(prefix) {
                break;
            }
            last_key = Some(key.clone());
            txn_delete(txn, key)
                .await
                .map_err(|e| SqlError::Internal(anyhow::anyhow!(e)))?;
        }
        if (count as u32) < DELTA_SCAN_BATCH_SIZE {
            break;
        }
        match last_key {
            Some(mut lk) => {
                lk.push(0x00);
                start = lk;
            }
            None => break,
        }
    }
    Ok(())
}

pub async fn get_or_alloc_rowid(
    txn: &mut Transaction,
    store: &TikvStore,
    db_id: u64,
    table_id: u64,
    pk_bytes: &[u8],
) -> Result<u64, SqlError> {
    if let Some(rowid) = get_rowid_for_pk(txn, db_id, table_id, pk_bytes).await? {
        return Ok(rowid);
    }
    let rowid = store
        .alloc_hnsw_rowid(db_id, table_id)
        .await
        .map_err(SqlError::Internal)?;
    put_rowid_mapping(txn, db_id, table_id, pk_bytes, rowid).await?;
    Ok(rowid)
}
