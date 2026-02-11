use super::*;

impl TikvStore {
    pub async fn create_procedure(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        name: &str,
        definition: &str,
    ) -> Result<()> {
        let key = self.key(&encode_procedure_key_v2(db_id, name));
        if txn.get(key.clone()).await?.is_some() {
            return Err(anyhow!("Procedure '{}' already exists", name));
        }
        txn_put(txn, key, definition.as_bytes().to_vec()).await?;
        info!("Created procedure '{}'", name);
        Ok(())
    }

    pub async fn get_procedure(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        name: &str,
    ) -> Result<Option<String>> {
        let key = self.key(&encode_procedure_key_v2(db_id, name));
        match txn.get(key).await? {
            Some(data) => Ok(Some(String::from_utf8(data)?)),
            None => Ok(None),
        }
    }

    pub async fn drop_procedure(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        name: &str,
    ) -> Result<bool> {
        let key = self.key(&encode_procedure_key_v2(db_id, name));
        if txn.get(key.clone()).await?.is_some() {
            txn_delete(txn, key).await?;
            info!("Dropped procedure '{}'", name);
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub async fn replace_procedure(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        name: &str,
        definition: &str,
    ) -> Result<()> {
        let key = self.key(&encode_procedure_key_v2(db_id, name));
        txn_put(txn, key, definition.as_bytes().to_vec()).await?;
        info!("Replaced procedure '{}'", name);
        Ok(())
    }
}
