//! Snapshot-pinned table scan for export.
//!
//! Reads table data at a specific `snapshot_ts` using short-lived TiKV
//! snapshot readers. Each page opens its own snapshot — this is safe because
//! all snapshots share the same `snapshot_ts` and the service safe point
//! prevents GC from reclaiming the MVCC versions.
//!
//! The scan is paginated (1024 rows per page) to avoid gRPC message size
//! limits and to enable streaming without materializing the whole table.

use anyhow::{anyhow, Context, Result};
use tikv_client::{BoundRange, KvPair, TimestampExt, TransactionClient, TransactionOptions};
use tracing::debug;

use crate::model::Row;
use crate::storage::backpressure::tikv_op;
use crate::storage::{deserialize_row, encode_table_data_range_v2};

/// Batch size for paginated export scan (row count).
///
/// TODO(S1-COPY-OUT): When wiring to COPY OUT transport, add a per-batch
/// byte cap (≤64 MB) alongside this row-count limit. Wide rows (JSONB,
/// BYTEA, TEXT) can push a 1024-row page well beyond the envelope.
const EXPORT_SCAN_BATCH_SIZE: u32 = 1024;

/// Result of scanning one page of table data.
pub(crate) struct ExportScanPage {
    /// Deserialized rows from this page.
    pub rows: Vec<Row>,
    /// Cursor for the next page. `None` means scan is exhausted.
    pub next_cursor: Option<Vec<u8>>,
}

/// Scan one page of table data at a pinned snapshot timestamp.
///
/// Opens a read-only snapshot at `snapshot_ts`, scans from `start_key` to
/// `end_key`, and returns up to `EXPORT_SCAN_BATCH_SIZE` rows plus a cursor
/// for the next page.
///
/// # Arguments
/// - `client`: TiKV transaction client (for creating snapshot)
/// - `snapshot_ts`: The pinned snapshot timestamp (from export snapshot)
/// - `start_key`: Start of the scan range (inclusive)
/// - `end_key`: End of the scan range (exclusive)
pub(crate) async fn scan_table_page(
    client: &TransactionClient,
    snapshot_ts: u64,
    start_key: Vec<u8>,
    end_key: Vec<u8>,
) -> Result<ExportScanPage> {
    let ts = tikv_client::Timestamp::from_version(snapshot_ts);
    let mut snap = client.snapshot(ts, TransactionOptions::new_optimistic());

    let range: BoundRange = (start_key..end_key.clone()).into();
    let iter =
        tikv_op!(snap.scan(range, EXPORT_SCAN_BATCH_SIZE).await).context("export scan page")?;
    let pairs: Vec<KvPair> = iter.collect();

    let mut rows = Vec::with_capacity(pairs.len());
    let mut last_key: Option<Vec<u8>> = None;

    for pair in &pairs {
        let key_bytes: &[u8] = pair.key().as_ref().into();
        last_key = Some(key_bytes.to_vec());
        let row = deserialize_row(pair.value())?;
        rows.push(row);
    }

    let next_cursor = if (pairs.len() as u32) < EXPORT_SCAN_BATCH_SIZE {
        None
    } else {
        last_key.map(|mut k| {
            k.push(0x00);
            k
        })
    };

    Ok(ExportScanPage { rows, next_cursor })
}

/// Scan an entire table at a pinned snapshot timestamp, invoking a callback
/// for each page of rows.
///
/// This is the primary entry point for whole-table export. It handles
/// pagination internally, calling `on_page` for each batch of rows.
/// The callback can perform streaming I/O (e.g., COPY protocol encoding)
/// without accumulating the entire table in memory.
///
/// # Arguments
/// - `client`: TiKV transaction client
/// - `snapshot_ts`: The pinned snapshot timestamp
/// - `db_id`: Database ID
/// - `table_id`: Table ID (from schema lookup)
/// - `on_page`: Async callback invoked for each page of rows
pub(crate) async fn export_table_scan<F, Fut>(
    client: &TransactionClient,
    snapshot_ts: u64,
    db_id: u64,
    table_id: u64,
    mut on_page: F,
) -> Result<u64>
where
    F: FnMut(Vec<Row>) -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    let (raw_start, raw_end) = encode_table_data_range_v2(db_id, table_id);
    let mut cursor = raw_start;
    let mut total_rows: u64 = 0;

    loop {
        let page = scan_table_page(client, snapshot_ts, cursor, raw_end.clone()).await?;
        let page_size = page.rows.len() as u64;

        if page_size > 0 {
            on_page(page.rows).await?;
            total_rows += page_size;
        }

        match page.next_cursor {
            Some(next) => cursor = next,
            None => break,
        }
    }

    debug!(db_id, table_id, total_rows, "export table scan complete");
    Ok(total_rows)
}

/// Look up a table schema at a pinned snapshot timestamp.
///
/// Creates a short-lived snapshot reader to fetch the schema, ensuring
/// the DDL view is consistent with the data snapshot.
pub(crate) async fn get_schema_at_snapshot(
    client: &TransactionClient,
    snapshot_ts: u64,
    db_id: u64,
    table_name: &str,
) -> Result<crate::model::TableSchema> {
    let ts = tikv_client::Timestamp::from_version(snapshot_ts);
    let mut snap = client.snapshot(ts, TransactionOptions::new_optimistic());

    let schema_key = crate::storage::encode_schema_key_v2(db_id, table_name);
    let data = tikv_op!(snap.get(schema_key).await).context("get schema at snapshot")?;

    let Some(data) = data else {
        return Err(anyhow!("table '{}' not found at snapshot", table_name));
    };

    crate::storage::deserialize_schema(&data).context("deserialize schema at snapshot")
}
