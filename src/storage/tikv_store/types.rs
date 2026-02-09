use super::*;

impl TikvStore {
    pub async fn create_type(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        def: UserTypeDef,
    ) -> Result<()> {
        let full_name = format!("{}.{}", def.schema, def.name);
        let key = self.key(&encode_type_key_v2(db_id, &full_name));
        if txn.get(key.clone()).await?.is_some() {
            return Err(anyhow!("Type '{}' already exists", full_name));
        }
        let data = bincode::serialize(&def).context("Failed to serialize type definition")?;
        txn_put(txn, key, data).await?;
        Ok(())
    }

    pub async fn get_type(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        full_name: &str,
    ) -> Result<Option<UserTypeDef>> {
        let key = self.key(&encode_type_key_v2(db_id, full_name));
        match txn.get(key).await? {
            Some(data) => Ok(Some(
                bincode::deserialize(&data).context("Failed to deserialize type definition")?,
            )),
            None => Ok(None),
        }
    }

    pub async fn list_types(&self, txn: &mut Transaction, db_id: u64) -> Result<Vec<UserTypeDef>> {
        let prefix = encode_type_prefix_v2(db_id);
        let mut end = prefix.clone();
        end.push(0xFF);
        let range: BoundRange = (prefix..end).into();
        let pairs = txn.scan(range, SCAN_LIMIT).await?;

        let mut types = Vec::new();
        for pair in pairs {
            let def: UserTypeDef =
                bincode::deserialize(pair.value()).context("Failed to deserialize type")?;
            types.push(def);
        }
        Ok(types)
    }

    pub async fn drop_type(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        full_name: &str,
    ) -> Result<bool> {
        let key = self.key(&encode_type_key_v2(db_id, full_name));
        if txn.get(key.clone()).await?.is_some() {
            txn_delete(txn, key).await?;
            Ok(true)
        } else {
            Ok(false)
        }
    }
}
