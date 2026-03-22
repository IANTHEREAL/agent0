//! Snapshot-pinned table scan for export.
//!
//! Reads table data at a specific `snapshot_ts` using short-lived TiKV
//! snapshot readers. Each page opens its own snapshot — this is safe because
//! all snapshots share the same `snapshot_ts` and the service safe point
//! prevents GC from reclaiming the MVCC versions.
//!
//! # Memory model (two-tier)
//!
//! - **Per-page peak memory**: bounded by `min(EXPORT_SCAN_PAGE_SIZE rows,
//!   TiKV gRPC response size limit)`. This is a row-count bound, not a byte
//!   bound — a page of wide rows may exceed `EXPORT_BATCH_BYTE_CAP`.
//! - **Cross-page flush threshold** (`EXPORT_BATCH_BYTE_CAP`, 64 MB): rows
//!   are accumulated across TiKV pages and flushed to the caller when
//!   cumulative value bytes reach this threshold. This bounds how much data
//!   is buffered between flushes, but does not bound per-page fetch size.
//!
//! A future improvement (S2) is adaptive page sizing: observe average row
//! size from the previous page and reduce `EXPORT_SCAN_PAGE_SIZE` for the
//! next fetch to keep per-page memory under the byte cap.

use anyhow::{anyhow, Context, Result};
use tikv_client::{BoundRange, KvPair, TimestampExt, TransactionClient, TransactionOptions};
use tracing::debug;

use crate::model::Row;
use crate::storage::backpressure::tikv_op;
use crate::storage::{deserialize_row, encode_table_data_range_v2};

/// Maximum rows per TiKV scan request. This bounds the gRPC response size
/// from TiKV. The actual batch emitted to callers may be smaller due to
/// the byte cap below.
const EXPORT_SCAN_PAGE_SIZE: u32 = 1024;

/// Cross-page flush threshold (64 MB). When cumulative serialized value
/// bytes across TiKV pages reach this threshold, the accumulated batch is
/// emitted to the caller and a new batch starts. A single row that exceeds
/// this limit is still emitted (we cannot split a row).
///
/// NOTE: This is a **flush threshold**, not a per-page fetch cap. A single
/// TiKV page (up to `EXPORT_SCAN_PAGE_SIZE` rows) may exceed this value
/// if individual rows are wide. See module-level docs for the full memory
/// model.
pub(crate) const EXPORT_BATCH_BYTE_CAP: usize = 64 * 1024 * 1024;

/// Result of scanning one page of table data from TiKV.
pub(crate) struct ExportScanPage {
    /// Deserialized rows from this page.
    pub rows: Vec<Row>,
    /// Approximate byte size of the raw KV values in this page.
    pub value_bytes: usize,
    /// Cursor for the next page. `None` means scan is exhausted.
    pub next_cursor: Option<Vec<u8>>,
}

/// Scan one page of table data at a pinned snapshot timestamp.
///
/// Opens a read-only snapshot at `snapshot_ts`, scans from `start_key` to
/// `end_key`, and returns up to `EXPORT_SCAN_PAGE_SIZE` rows plus a cursor
/// for the next page.
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
        tikv_op!(snap.scan(range, EXPORT_SCAN_PAGE_SIZE).await).context("export scan page")?;
    let pairs: Vec<KvPair> = iter.collect();

    let mut rows = Vec::with_capacity(pairs.len());
    let mut last_key: Option<Vec<u8>> = None;
    let mut value_bytes: usize = 0;

    for pair in &pairs {
        let key_bytes: &[u8] = pair.key().as_ref().into();
        last_key = Some(key_bytes.to_vec());
        value_bytes += pair.value().len();
        let row = deserialize_row(pair.value())?;
        rows.push(row);
    }

    let next_cursor = if (pairs.len() as u32) < EXPORT_SCAN_PAGE_SIZE {
        None
    } else {
        last_key.map(|mut k| {
            k.push(0x00);
            k
        })
    };

    Ok(ExportScanPage {
        rows,
        value_bytes,
        next_cursor,
    })
}

/// Scan an entire table at a pinned snapshot timestamp, invoking a callback
/// for each batch of rows.
///
/// This is the primary entry point for whole-table export. It handles
/// pagination internally, calling `on_batch` for each batch of rows.
/// The callback can perform streaming I/O (e.g., COPY protocol encoding)
/// without accumulating the entire table in memory.
///
/// Batches are bounded by both row count (`EXPORT_SCAN_PAGE_SIZE`) and
/// byte size (`EXPORT_BATCH_BYTE_CAP`). When cumulative value bytes
/// across TiKV pages reach the byte cap, the accumulated rows are flushed
/// to `on_batch` and a new batch starts.
///
/// # Backpressure
///
/// The callback is `await`ed before the next TiKV page is fetched. A slow
/// consumer (e.g., a client reading COPY data slowly) blocks the scan,
/// preventing unbounded buffering.
pub(crate) async fn export_table_scan<F, Fut>(
    client: &TransactionClient,
    snapshot_ts: u64,
    db_id: u64,
    table_id: u64,
    mut on_batch: F,
) -> Result<u64>
where
    F: FnMut(Vec<Row>) -> Fut,
    Fut: std::future::Future<Output = Result<()>>,
{
    let (raw_start, raw_end) = encode_table_data_range_v2(db_id, table_id);
    let mut cursor = raw_start;
    let mut total_rows: u64 = 0;

    // Accumulate rows across TiKV pages until the byte cap is reached.
    let mut batch_rows: Vec<Row> = Vec::new();
    let mut batch_bytes: usize = 0;

    loop {
        let page = scan_table_page(client, snapshot_ts, cursor, raw_end.clone()).await?;
        let page_row_count = page.rows.len() as u64;
        let page_exhausted = page.next_cursor.is_none();

        if page_row_count > 0 {
            batch_bytes += page.value_bytes;
            batch_rows.extend(page.rows);
            total_rows += page_row_count;
        }

        // Flush batch if byte cap reached or scan exhausted.
        if !batch_rows.is_empty() && (batch_bytes >= EXPORT_BATCH_BYTE_CAP || page_exhausted) {
            on_batch(std::mem::take(&mut batch_rows)).await?;
            batch_bytes = 0;
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
