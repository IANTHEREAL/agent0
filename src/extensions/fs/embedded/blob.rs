use crate::extensions::fs::embedded::keys;
use anyhow::{anyhow, Result};
use tikv_client::Transaction;

pub(crate) async fn read_blob(txn: &mut Transaction, inode_id: u64, size: u64) -> Result<Vec<u8>> {
    let mut data = txn
        .get(keys::blob_key(inode_id))
        .await?
        .ok_or_else(|| anyhow!("fs9: inline blob missing for inode {inode_id}"))?;

    // InlineBlob supports sparse-like semantics by interpreting missing tail bytes as zeros.
    let expected = usize::try_from(size)
        .map_err(|_| anyhow!("fs9: inode size {size} exceeds addressable memory"))?;
    if data.len() < expected {
        data.resize(expected, 0);
    } else if data.len() > expected {
        data.truncate(expected);
    }
    Ok(data)
}

pub(crate) async fn write_blob(txn: &mut Transaction, inode_id: u64, data: &[u8]) -> Result<()> {
    let key = keys::blob_key(inode_id);
    crate::txn::check_value_size(&key, data)?;
    txn.put(key, data.to_vec()).await?;
    Ok(())
}

pub(crate) async fn delete_blob(txn: &mut Transaction, inode_id: u64) -> Result<()> {
    txn.delete(keys::blob_key(inode_id)).await?;
    Ok(())
}

pub(crate) fn apply_write_at(blob: &mut Vec<u8>, offset: u64, data: &[u8]) -> Result<()> {
    if data.is_empty() {
        return Ok(());
    }

    let offset_usize = usize::try_from(offset)
        .map_err(|_| anyhow!("fs9: write offset {offset} exceeds addressable memory"))?;
    let end = offset_usize
        .checked_add(data.len())
        .ok_or_else(|| anyhow!("fs9: write range overflow"))?;

    if blob.len() < offset_usize {
        blob.resize(offset_usize, 0);
    }
    if blob.len() < end {
        blob.resize(end, 0);
    }
    blob[offset_usize..end].copy_from_slice(data);
    Ok(())
}

pub(crate) fn apply_truncate(blob: &mut Vec<u8>, size: u64) -> Result<()> {
    let target =
        usize::try_from(size).map_err(|_| anyhow!("fs9: truncate size {size} exceeds memory"))?;
    if blob.len() < target {
        blob.resize(target, 0);
    } else {
        blob.truncate(target);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_write_at_writes_and_grows() {
        let mut b = Vec::new();
        apply_write_at(&mut b, 2, b"ab").unwrap();
        assert_eq!(b, vec![0, 0, b'a', b'b']);
    }

    #[test]
    fn apply_write_at_overwrites() {
        let mut b = b"hello".to_vec();
        apply_write_at(&mut b, 1, b"i").unwrap();
        assert_eq!(b, b"hillo");
    }

    #[test]
    fn apply_truncate_shrinks_and_grows_with_zeros() {
        let mut b = b"hello".to_vec();
        apply_truncate(&mut b, 2).unwrap();
        assert_eq!(b, b"he");
        apply_truncate(&mut b, 5).unwrap();
        assert_eq!(b, b"he\0\0\0");
    }
}
