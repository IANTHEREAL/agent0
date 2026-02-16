use super::*;
use crate::sql::optimizer::statistics::TableStatistics;

impl TikvStore {
    /// Persist table statistics to TiKV.
    pub async fn store_statistics(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        stats: &TableStatistics,
    ) -> Result<()> {
        let key = self.key(&encode_stats_key_v2(db_id, stats.table_id));
        let data = bincode::serialize(stats).context("Failed to serialize table statistics")?;
        txn_put(txn, key, data).await
    }

    /// Load table statistics from TiKV. Returns `None` if no statistics exist.
    pub async fn load_statistics(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        table_id: u64,
    ) -> Result<Option<TableStatistics>> {
        let key = self.key(&encode_stats_key_v2(db_id, table_id));
        match txn.get(key).await? {
            Some(data) => {
                let stats: TableStatistics = bincode::deserialize(&data)
                    .context("Failed to deserialize table statistics")?;
                Ok(Some(stats))
            }
            None => Ok(None),
        }
    }

    /// Delete persisted statistics for a table. Called during DROP TABLE cleanup.
    pub async fn delete_statistics(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        table_id: u64,
    ) -> Result<()> {
        let key = self.key(&encode_stats_key_v2(db_id, table_id));
        txn_delete(txn, key).await
    }
}
