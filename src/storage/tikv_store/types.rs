use super::*;
use crate::sql::error::SqlError;

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
            return Err(
                SqlError::DuplicateObject(format!("Type '{}' already exists", full_name)).into(),
            );
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

    /// Overwrite an existing type definition in-place (same key).
    pub async fn update_type(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        def: UserTypeDef,
    ) -> Result<()> {
        let full_name = format!("{}.{}", def.schema, def.name);
        let key = self.key(&encode_type_key_v2(db_id, &full_name));
        let data = bincode::serialize(&def).context("Failed to serialize type definition")?;
        txn_put(txn, key, data).await?;
        Ok(())
    }

    /// Rename a type: delete old key, write new key, return updated def.
    pub async fn rename_type(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        old_full: &str,
        new_full: &str,
        mut def: UserTypeDef,
    ) -> Result<UserTypeDef> {
        let old_key = self.key(&encode_type_key_v2(db_id, old_full));
        let new_key = self.key(&encode_type_key_v2(db_id, new_full));
        if txn.get(new_key.clone()).await?.is_some() {
            return Err(
                SqlError::DuplicateObject(format!("type \"{}\" already exists", new_full)).into(),
            );
        }
        // Update the def's name field to match the new name.
        def.name = new_full
            .splitn(2, '.')
            .nth(1)
            .unwrap_or(new_full)
            .to_string();
        let data = bincode::serialize(&def).context("Failed to serialize type definition")?;
        txn_delete(txn, old_key).await?;
        txn_put(txn, new_key, data).await?;
        Ok(def)
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
