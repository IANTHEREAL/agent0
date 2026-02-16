use super::*;

impl TikvStore {
    pub async fn record_migration(
        &self,
        txn: &mut Transaction,
        record: MigrationRecord,
    ) -> Result<()> {
        let key = self.key(&encode_migration_key(&record.name));
        let data = bincode::serialize(&record).context("Failed to serialize migration record")?;
        txn_put(txn, key, data).await?;
        Ok(())
    }

    pub async fn list_migrations(&self, txn: &mut Transaction) -> Result<Vec<MigrationRecord>> {
        let prefix = encode_migration_prefix();
        let mut end = prefix.clone();
        end.push(0xFF);
        let range: BoundRange = (prefix.clone()..end).into();
        let pairs = txn.scan(range, SCAN_LIMIT).await?;

        let mut migrations = Vec::new();
        for pair in pairs {
            let key: &[u8] = pair.key().as_ref().into();
            if !key.starts_with(&prefix) {
                continue;
            }
            let record: MigrationRecord =
                bincode::deserialize(pair.value()).context("Failed to deserialize migration")?;
            migrations.push(record);
        }

        migrations.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(migrations)
    }
}
