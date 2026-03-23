use crate::model::DataType;
use crate::sql::query_context::QueryContext;
use pgwire::error::{ErrorInfo, PgWireError, PgWireResult};
use std::collections::{HashMap, HashSet};

pub(in crate::protocol::handler) fn copy_from_stdin_line_too_long_error() -> PgWireError {
    PgWireError::UserError(Box::new(ErrorInfo::new(
        "ERROR".to_string(),
        "54000".to_string(),
        format!(
            "COPY FROM STDIN row exceeded max size ({} bytes)",
            MAX_COPY_FROM_STDIN_LINE_BYTES
        ),
    )))
}

/// Transaction rotation threshold for COPY FROM STDIN (rows per commit).
/// Matches the Parquet COPY and DDL backfill paths (5000).
pub const COPY_STDIN_COMMIT_SIZE: usize = 5000;

/// Hard ceiling: if rotation is blocked (e.g. by self-FK forward references)
/// and the transaction exceeds this many rows, COPY aborts with a clear error
/// instead of silently growing until TiKV rejects the transaction.
pub const COPY_STDIN_MAX_UNROTATED_ROWS: usize = 50_000;

pub struct CopyContext {
    pub table_name: String,
    pub columns: Vec<String>,
    pub column_types: Vec<Option<DataType>>,
    pub copy_options: crate::protocol::copy_format::CopyOptions,
    pub query_context: QueryContext,
    pub runtime_context: crate::sql::runtime_context::StatementRuntimeContext,
    pub backpressure_guard: Option<crate::storage::backpressure::BackpressureGuard>,
    pub line_buffer: Vec<u8>,
    pub row_count: usize,
    pub started_txn: bool,
    pub reached_end_marker: bool,
    /// Set to true after the header row has been consumed.
    pub header_skipped: bool,
    /// Rows inserted since the last transaction commit (for rotation tracking).
    pub batch_rows_since_commit: usize,
    /// Accumulated self-referencing FK ref-column keys (PK side) across all
    /// CopyData chunks.  Keyed by FK constraint name.
    pub pending_self_fk_keys: HashMap<String, HashSet<String>>,
    /// Accumulated unresolved self-referencing FK checks that need deferred
    /// validation at CopyDone.
    /// Each entry is `(constraint_id, fk_name, hash_key, display_values)`.
    pub deferred_self_fk_checks: Vec<(usize, String, String, String)>,
}

/// A safety cap to prevent unbounded buffering if the client sends a single row without newlines.
/// This is a per-row cap (not a cap on the total COPY stream).
const MAX_COPY_FROM_STDIN_LINE_BYTES: usize = 32 * 1024 * 1024;

impl CopyContext {
    pub(in crate::protocol::handler) fn push_copy_data(
        &mut self,
        data: &[u8],
    ) -> PgWireResult<Vec<Vec<u8>>> {
        let mut lines: Vec<Vec<u8>> = Vec::new();
        let mut start = 0usize;

        for (idx, byte) in data.iter().enumerate() {
            if *byte != b'\n' {
                continue;
            }

            let mut line: Vec<u8> = Vec::new();
            if !self.line_buffer.is_empty() {
                line.extend_from_slice(&self.line_buffer);
                self.line_buffer.clear();
            }
            line.extend_from_slice(&data[start..idx]);
            if line.last() == Some(&b'\r') {
                line.pop();
            }

            if line.len() > MAX_COPY_FROM_STDIN_LINE_BYTES {
                return Err(copy_from_stdin_line_too_long_error());
            }

            lines.push(line);
            start = idx.saturating_add(1);
        }

        if start < data.len() {
            let remaining = &data[start..];
            let new_len = self.line_buffer.len().saturating_add(remaining.len());
            if new_len > MAX_COPY_FROM_STDIN_LINE_BYTES {
                return Err(copy_from_stdin_line_too_long_error());
            }
            self.line_buffer.extend_from_slice(remaining);
        }

        Ok(lines)
    }

    pub(in crate::protocol::handler) fn drain_final_line(&mut self) -> Option<Vec<u8>> {
        if self.line_buffer.is_empty() {
            return None;
        }
        let mut line = Vec::new();
        std::mem::swap(&mut line, &mut self.line_buffer);
        if line.last() == Some(&b'\r') {
            line.pop();
        }
        Some(line)
    }
}
