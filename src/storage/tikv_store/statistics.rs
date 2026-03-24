use super::*;
use crate::sql::optimizer::statistics::{ColumnStatistics, TableStatistics, TableStatsHeader};
use crate::storage::backpressure::tikv_op;

impl TikvStore {
    /// Persist table statistics to TiKV using per-column storage.
    ///
    /// Stores a table-level header KV (row_count, last_analyzed) plus one
    /// KV per column's `ColumnStatistics`.  This avoids the single-blob
    /// problem where wide tables with TEXT columns exceeded TiKV's
    /// raft-entry-max-size.
    ///
    /// Also deletes the legacy single-blob key if it exists, so that
    /// `load_statistics` does not fall back to stale legacy data after
    /// a re-ANALYZE.
    pub async fn store_statistics(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        stats: &TableStatistics,
    ) -> Result<()> {
        // 1. Write table-level header.
        let header = TableStatsHeader {
            table_id: stats.table_id,
            row_count: stats.row_count,
            last_analyzed: stats.last_analyzed,
        };
        let header_key = self.key(&encode_stats_header_key(db_id, stats.table_id));
        let header_data =
            bincode::serialize(&header).context("Failed to serialize stats header")?;
        txn_put(txn, header_key, header_data).await?;

        // 2. Write each column's statistics as a separate KV.
        for (col_name, col_stats) in &stats.columns {
            let col_key = self.key(&encode_stats_column_key(db_id, stats.table_id, col_name));
            let col_data =
                bincode::serialize(col_stats).context("Failed to serialize column statistics")?;
            txn_put(txn, col_key, col_data).await?;
        }

        // 3. Delete legacy single-blob key (if it exists from a prior version).
        let legacy_key = self.key(&encode_stats_key_v2(db_id, stats.table_id));
        txn_delete(txn, legacy_key).await?;

        Ok(())
    }

    /// Load table statistics from TiKV.
    ///
    /// Tries the per-column format first (header key + column prefix scan).
    /// Falls back to the legacy single-blob format for backward compatibility
    /// with data written before the per-column migration.
    pub async fn load_statistics(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        table_id: u64,
    ) -> Result<Option<TableStatistics>> {
        // Try per-column format first.
        let header_key = self.key(&encode_stats_header_key(db_id, table_id));
        if let Some(header_data) = tikv_op!(txn.get(header_key).await)? {
            let header: TableStatsHeader =
                bincode::deserialize(&header_data).context("Failed to deserialize stats header")?;

            // Scan column stats keys in pages to avoid gRPC message size limits.
            // Each column stats KV is ~100-120 KB max (with WIDTH_THRESHOLD=1024),
            // so 50 columns per page ≈ 5-6 MB, safely under gRPC limits.
            const STATS_SCAN_PAGE_SIZE: u32 = 50;

            let col_prefix = self.key(&encode_stats_column_prefix(db_id, table_id));
            let col_prefix_end = {
                let mut end = col_prefix.clone();
                if let Some(last) = end.last_mut() {
                    *last = last.wrapping_add(1);
                }
                end
            };

            let mut columns = std::collections::HashMap::new();
            let mut scan_start = col_prefix.clone();
            loop {
                let pairs: Vec<_> = tikv_op!(
                    txn.scan(
                        scan_start.clone()..col_prefix_end.clone(),
                        STATS_SCAN_PAGE_SIZE
                    )
                    .await
                )?
                .collect();
                if pairs.is_empty() {
                    break;
                }
                let mut last_key: Option<Vec<u8>> = None;
                for pair in &pairs {
                    let key_bytes: Vec<u8> = pair.key().clone().into();
                    if key_bytes.len() > col_prefix.len() {
                        let col_name_bytes = &key_bytes[col_prefix.len()..];
                        if let Ok(col_name) = std::str::from_utf8(col_name_bytes) {
                            let col_stats: ColumnStatistics = bincode::deserialize(pair.value())
                                .context("Failed to deserialize column statistics")?;
                            columns.insert(col_name.to_string(), col_stats);
                        }
                    }
                    last_key = Some(key_bytes);
                }
                if (pairs.len() as u32) < STATS_SCAN_PAGE_SIZE {
                    break; // Last page
                }
                // Next page starts after the last key.
                if let Some(mut next) = last_key {
                    next.push(0);
                    scan_start = next;
                } else {
                    break;
                }
            }

            return Ok(Some(TableStatistics {
                table_id: header.table_id,
                row_count: header.row_count,
                last_analyzed: header.last_analyzed,
                columns,
            }));
        }

        // Fallback: try legacy single-blob format.
        let legacy_key = self.key(&encode_stats_key_v2(db_id, table_id));
        match tikv_op!(txn.get(legacy_key).await)? {
            Some(data) => {
                let stats: TableStatistics = bincode::deserialize(&data)
                    .context("Failed to deserialize table statistics (legacy format)")?;
                Ok(Some(stats))
            }
            None => Ok(None),
        }
    }

    /// Delete persisted statistics for a table. Called during DROP TABLE cleanup.
    ///
    /// Deletes both the per-column format (header + all column keys) and the
    /// legacy single-blob key.
    pub async fn delete_statistics(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        table_id: u64,
    ) -> Result<()> {
        // Delete per-column header.
        let header_key = self.key(&encode_stats_header_key(db_id, table_id));
        txn_delete(txn, header_key).await?;

        // Delete all per-column stats keys via paginated prefix scan.
        const STATS_DELETE_PAGE_SIZE: u32 = 100;

        let col_prefix = self.key(&encode_stats_column_prefix(db_id, table_id));
        let col_prefix_end = {
            let mut end = col_prefix.clone();
            if let Some(last) = end.last_mut() {
                *last = last.wrapping_add(1);
            }
            end
        };
        let mut scan_start = col_prefix;
        loop {
            let pairs: Vec<_> = tikv_op!(
                txn.scan(
                    scan_start.clone()..col_prefix_end.clone(),
                    STATS_DELETE_PAGE_SIZE
                )
                .await
            )?
            .collect();
            if pairs.is_empty() {
                break;
            }
            let mut last_key: Option<Vec<u8>> = None;
            for pair in &pairs {
                let key: Vec<u8> = pair.key().clone().into();
                last_key = Some(key.clone());
                txn_delete(txn, key).await?;
            }
            if (pairs.len() as u32) < STATS_DELETE_PAGE_SIZE {
                break;
            }
            if let Some(mut next) = last_key {
                next.push(0);
                scan_start = next;
            } else {
                break;
            }
        }

        // Delete legacy single-blob key.
        let legacy_key = self.key(&encode_stats_key_v2(db_id, table_id));
        txn_delete(txn, legacy_key).await?;

        Ok(())
    }
}
