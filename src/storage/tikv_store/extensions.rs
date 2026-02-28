use super::*;
use crate::storage::backpressure::tikv_op;

impl TikvStore {
    pub async fn get_extension(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        ext_name: &str,
    ) -> Result<Option<InstalledExtension>> {
        let key = self.key(&encode_extension_key_v2(db_id, ext_name));
        match tikv_op!(txn.get(key).await)? {
            Some(data) => Ok(Some(
                bincode::deserialize(&data).context("Failed to deserialize extension")?,
            )),
            None => Ok(None),
        }
    }

    pub async fn put_extension(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        ext: &InstalledExtension,
    ) -> Result<()> {
        let key = self.key(&encode_extension_key_v2(db_id, &ext.name));
        let data = bincode::serialize(ext).context("Failed to serialize extension")?;
        txn_put(txn, key, data).await?;
        Ok(())
    }

    pub async fn drop_extension(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        ext_name: &str,
    ) -> Result<bool> {
        let key = self.key(&encode_extension_key_v2(db_id, ext_name));
        let existed = tikv_op!(txn.get(key.clone()).await)?.is_some();
        if !existed {
            return Ok(false);
        }

        txn_delete(txn, key).await?;
        let cfg_key = self.key(&encode_extension_config_key_v2(db_id, ext_name));
        let _ = txn_delete(txn, cfg_key).await;
        let comment_key = self.key(&encode_comment_extension_key_v2(db_id, ext_name));
        txn_delete(txn, comment_key).await?;
        Ok(true)
    }

    pub async fn list_extensions(
        &self,
        txn: &mut Transaction,
        db_id: u64,
    ) -> Result<Vec<InstalledExtension>> {
        let prefix = encode_extension_prefix_v2(db_id);
        let mut end = prefix.clone();
        end.push(0xFF);
        let range: BoundRange = (prefix.clone()..end).into();
        let pairs = tikv_op!(txn.scan(range, SCAN_LIMIT).await)?;

        let mut exts = Vec::new();
        for pair in pairs {
            let key: &[u8] = pair.key().as_ref().into();
            if !key.starts_with(&prefix) {
                continue;
            }
            let ext: InstalledExtension =
                bincode::deserialize(pair.value()).context("Failed to deserialize extension")?;
            exts.push(ext);
        }
        Ok(exts)
    }
}
