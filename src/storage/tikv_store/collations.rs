use super::*;
use crate::sql::collation::CollationDef;
use crate::sql::error::SqlError;
use crate::storage::backpressure::tikv_op;

impl TikvStore {
    pub async fn create_collation(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        def: &CollationDef,
    ) -> Result<()> {
        let key = self.key(&encode_collation_key_v2(db_id, &def.name));
        if tikv_op!(txn.get(key.clone()).await)?.is_some() {
            return Err(SqlError::DuplicateObject(format!(
                "collation \"{}\" already exists",
                def.name
            ))
            .into());
        }
        let data = bincode::serialize(def).context("Failed to serialize collation definition")?;
        txn_put(txn, key, data).await?;
        Ok(())
    }

    pub async fn list_collations(
        &self,
        txn: &mut Transaction,
        db_id: u64,
    ) -> Result<Vec<CollationDef>> {
        let prefix = encode_collation_prefix_v2(db_id);
        let mut end = prefix.clone();
        end.push(0xFF);
        let range: BoundRange = (prefix..end).into();
        let pairs = tikv_op!(txn.scan(range, SCAN_LIMIT).await)?;

        let mut collations = Vec::new();
        for pair in pairs {
            let def: CollationDef =
                bincode::deserialize(pair.value()).context("Failed to deserialize collation")?;
            collations.push(def);
        }
        Ok(collations)
    }

    pub async fn drop_collation(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        name: &str,
    ) -> Result<bool> {
        let key = self.key(&encode_collation_key_v2(db_id, name));
        if tikv_op!(txn.get(key.clone()).await)?.is_some() {
            txn_delete(txn, key).await?;
            Ok(true)
        } else {
            Ok(false)
        }
    }
}
