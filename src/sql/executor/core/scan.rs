//! Scan helpers

use super::*;

impl Executor {
    pub(crate) async fn scan_and_fill(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        table_name: &str,
        schema: &TableSchema,
    ) -> Result<Vec<Row>> {
        self.scan_and_fill_with_limit(txn, db_id, table_name, schema, None)
            .await
    }

    pub(crate) async fn scan_and_fill_with_limit(
        &self,
        txn: &mut Transaction,
        db_id: u64,
        table_name: &str,
        schema: &TableSchema,
        limit: Option<usize>,
    ) -> Result<Vec<Row>> {
        let rows = self.store.scan(txn, db_id, table_name, limit).await?;
        let mut filled_rows = Vec::with_capacity(rows.len());
        for mut row in rows {
            fill_row_defaults(&mut row, schema)?;
            filled_rows.push(row);
        }
        Ok(filled_rows)
    }
}
