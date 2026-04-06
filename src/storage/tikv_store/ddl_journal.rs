//! DDL intent journal for crash recovery.
//!
//! Non-atomic DDL operations (CREATE INDEX, CTAS) that span multiple TiKV
//! transactions record their intent in this journal *before* starting the
//! multi-batch work.  On successful completion the journal entry is deleted.
//! If the server crashes mid-operation, the startup reconciliation pass
//! (`reconcile_ddl_journal`) cleans up orphaned data using the journal.

use super::*;
use crate::storage::backpressure::tikv_op;

use serde::{Deserialize, Serialize};

/// A single DDL journal entry describing an in-flight multi-batch DDL.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DdlJournalEntry {
    pub id: u64,
    pub db_id: u64,
    pub operation: DdlOperation,
    pub created_at: u64,
}

/// The specific DDL operation recorded in the journal.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DdlOperation {
    CreateIndex {
        table_id: u64,
        index_id: u64,
        /// Fully-qualified index name for releasing the relation name reservation.
        index_name: String,
        /// The start key of the index KV range (inclusive).
        index_range_start: Vec<u8>,
        /// The end key of the index KV range (exclusive).
        index_range_end: Vec<u8>,
    },
    CreateTableAsSelect {
        table_id: u64,
        table_name: String,
    },
}

impl TikvStore {
    /// Persist a DDL journal entry inside the given transaction.
    pub async fn write_ddl_journal(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        entry: &DdlJournalEntry,
    ) -> Result<()> {
        let key = self.key(&encode_ddl_journal_key(db_id, entry.id));
        let data = bincode::serialize(entry).context("Failed to serialize DDL journal entry")?;
        txn_put(txn, key, data).await?;
        Ok(())
    }

    /// Remove a DDL journal entry (called after the DDL completes or on
    /// explicit cleanup).
    pub async fn delete_ddl_journal(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        journal_id: u64,
    ) -> Result<()> {
        let key = self.key(&encode_ddl_journal_key(db_id, journal_id));
        txn_delete(txn, key).await?;
        Ok(())
    }

    /// Scan all DDL journal entries for a given database.
    pub async fn scan_ddl_journal(
        &self,
        txn: &mut Transaction,
        db_id: u64,
    ) -> Result<Vec<DdlJournalEntry>> {
        let prefix = self.key(&encode_ddl_journal_prefix(db_id));
        let end = crate::storage::encoding::encode_prefix_end(&prefix);
        let range: BoundRange = (prefix.clone()..end).into();
        // Journal entries are few (one per in-flight DDL), so a small limit suffices.
        let pairs = tikv_op!(txn.scan(range, 1024).await)?;

        let mut entries = Vec::new();
        for pair in pairs {
            let entry: DdlJournalEntry = bincode::deserialize(pair.value())
                .context("Failed to deserialize DDL journal entry")?;
            entries.push(entry);
        }
        Ok(entries)
    }

    /// Delete one batch of keys in a range. Returns the next cursor position,
    /// or `None` if the range is exhausted.
    ///
    /// The caller is responsible for committing the transaction and opening a
    /// new one between batches to stay within TiKV mutation limits.
    pub async fn delete_key_range_batch(
        &self,
        txn: &mut Transaction,
        cursor: Vec<u8>,
        end: &[u8],
    ) -> Result<Option<Vec<u8>>> {
        const BATCH_SIZE: u32 = 1024;
        let range: BoundRange = (cursor..end.to_vec()).into();
        let pairs: Vec<tikv_client::KvPair> =
            tikv_op!(txn.scan(range, BATCH_SIZE).await)?.collect();
        if pairs.is_empty() {
            return Ok(None);
        }
        let mut last_key_bytes: Vec<u8> = Vec::new();
        for pair in pairs {
            let kb: Vec<u8> = pair.key().clone().into();
            last_key_bytes = kb.clone();
            txn_delete(txn, kb).await?;
        }
        // Advance cursor past the last key seen.
        last_key_bytes.push(0x00);
        Ok(Some(last_key_bytes))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ddl_journal_entry_serialization_round_trip() {
        let entry = DdlJournalEntry {
            id: 42,
            db_id: 7,
            operation: DdlOperation::CreateIndex {
                table_id: 100,
                index_id: 5,
                index_name: "public.idx_test".to_string(),
                index_range_start: vec![0xDE, 0xAD],
                index_range_end: vec![0xDE, 0xAE],
            },
            created_at: 1_700_000_000,
        };

        let data = bincode::serialize(&entry).unwrap();
        let decoded: DdlJournalEntry = bincode::deserialize(&data).unwrap();

        assert_eq!(decoded.id, 42);
        assert_eq!(decoded.db_id, 7);
        assert_eq!(decoded.created_at, 1_700_000_000);
        match &decoded.operation {
            DdlOperation::CreateIndex {
                table_id,
                index_id,
                index_name,
                index_range_start,
                index_range_end,
            } => {
                assert_eq!(*table_id, 100);
                assert_eq!(*index_id, 5);
                assert_eq!(index_name, "public.idx_test");
                assert_eq!(index_range_start, &[0xDE, 0xAD]);
                assert_eq!(index_range_end, &[0xDE, 0xAE]);
            }
            _ => panic!("expected CreateIndex variant"),
        }
    }

    #[test]
    fn ddl_journal_ctas_serialization_round_trip() {
        let entry = DdlJournalEntry {
            id: 99,
            db_id: 3,
            operation: DdlOperation::CreateTableAsSelect {
                table_id: 50,
                table_name: "my_table".to_string(),
            },
            created_at: 1_700_000_001,
        };

        let data = bincode::serialize(&entry).unwrap();
        let decoded: DdlJournalEntry = bincode::deserialize(&data).unwrap();

        assert_eq!(decoded.id, 99);
        assert_eq!(decoded.db_id, 3);
        match &decoded.operation {
            DdlOperation::CreateTableAsSelect {
                table_id,
                table_name,
            } => {
                assert_eq!(*table_id, 50);
                assert_eq!(table_name, "my_table");
            }
            _ => panic!("expected CreateTableAsSelect variant"),
        }
    }

    #[test]
    fn ddl_journal_key_encoding() {
        let key = encode_ddl_journal_key(1, 42);
        let prefix = encode_ddl_journal_prefix(1);
        assert!(key.starts_with(&prefix));
        // The key should be prefix + 8 bytes (u64 big-endian journal_id)
        assert_eq!(key.len(), prefix.len() + 8);
    }
}
