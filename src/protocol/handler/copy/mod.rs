use crate::model::DataType;
use crate::sql::query_context::QueryContext;
use pgwire::error::{ErrorInfo, PgWireError, PgWireResult};

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

pub(in crate::protocol::handler) fn copy_row_column_mismatch_error(
    actual: usize,
    expected: usize,
) -> PgWireError {
    PgWireError::UserError(Box::new(ErrorInfo::new(
        "ERROR".to_string(),
        "22P04".to_string(),
        format!(
            "COPY row has {} columns but {} columns expected",
            actual, expected
        ),
    )))
}

#[derive(Debug, Clone)]
pub struct CopyContext {
    pub table_name: String,
    pub columns: Vec<String>,
    pub column_types: Vec<Option<DataType>>,
    pub query_context: QueryContext,
    pub line_buffer: Vec<u8>,
    pub row_count: usize,
    pub started_txn: bool,
    pub reached_end_marker: bool,
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
