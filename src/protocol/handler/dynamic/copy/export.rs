//! Streaming export via COPY OUT protocol with snapshot-pinned reads.
//!
//! Handles `EXPORT SNAPSHOT COPY '<table_name>' '<snapshot_id>'` by streaming
//! table data at a pinned snapshot timestamp directly to the client using the
//! pgwire COPY OUT protocol. Each batch is flushed before the next TiKV page
//! is fetched, providing natural backpressure.
//!
//! Requires superuser. Validates snapshot is active and not expired.

use super::super::DynamicPgHandler;
use crate::export;
use crate::export::registry::ExportSnapshotState;
use crate::protocol::copy_format::{self, CopyOptions};
use futures::{Sink, SinkExt};
use pgwire::api::results::CopyResponse;
use pgwire::api::ClientInfo;
use pgwire::error::{ErrorInfo, PgWireError, PgWireResult};
use pgwire::messages::PgWireBackendMessage;
use std::fmt::Debug;
use tracing::debug;

impl DynamicPgHandler {
    /// Try to handle an `EXPORT SNAPSHOT COPY` command.
    ///
    /// Returns `Some(result)` if the query matched and was handled (or failed).
    /// Returns `None` if the query is not an EXPORT SNAPSHOT COPY command.
    pub(in crate::protocol::handler) async fn try_handle_export_snapshot_copy<'a, C>(
        &self,
        client: &mut C,
        query: &str,
    ) -> PgWireResult<Option<Vec<pgwire::api::results::Response<'a>>>>
    where
        C: ClientInfo + Sink<PgWireBackendMessage> + Unpin + Send + Sync,
        C::Error: Debug,
        PgWireError: From<<C as Sink<PgWireBackendMessage>>::Error>,
    {
        let rest = match strip_export_copy_prefix(query) {
            Some(r) => r,
            None => return Ok(None),
        };

        // Parse: EXPORT SNAPSHOT COPY '<table_name>' '<snapshot_id>'
        let (table_name, snapshot_id) = parse_export_copy_args(rest).map_err(|e| {
            PgWireError::UserError(Box::new(ErrorInfo::new(
                "ERROR".to_string(),
                "42601".to_string(),
                e.to_string(),
            )))
        })?;

        // Superuser gate.
        {
            let session = self.auth().session.lock().await;
            if !session.is_superuser() {
                return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                    "ERROR".to_string(),
                    "42501".to_string(),
                    "permission denied: EXPORT SNAPSHOT COPY requires superuser".to_string(),
                ))));
            }
        }

        // Look up snapshot and validate state.
        let registry = export::global_registry().ok_or_else(|| {
            PgWireError::UserError(Box::new(ErrorInfo::new(
                "ERROR".to_string(),
                "XX000".to_string(),
                "export snapshot subsystem not initialized".to_string(),
            )))
        })?;

        let snap = registry
            .get_export_snapshot(&snapshot_id)
            .await
            .map_err(|e| {
                PgWireError::UserError(Box::new(ErrorInfo::new(
                    "ERROR".to_string(),
                    "XX000".to_string(),
                    format!("get_export_snapshot failed: {e}"),
                )))
            })?
            .ok_or_else(|| {
                PgWireError::UserError(Box::new(ErrorInfo::new(
                    "ERROR".to_string(),
                    "02000".to_string(),
                    format!("export snapshot '{}' not found", snapshot_id),
                )))
            })?;

        // Validate snapshot is active.
        if snap.state != ExportSnapshotState::Active {
            return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                "ERROR".to_string(),
                "55000".to_string(),
                format!(
                    "export snapshot '{}' is {}, expected Active",
                    snapshot_id, snap.state
                ),
            ))));
        }

        // Validate snapshot has not expired (clock check, in case janitor hasn't run).
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;
        if now_ms > snap.expires_at_ms {
            return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                "ERROR".to_string(),
                "55000".to_string(),
                format!(
                    "export snapshot '{}' has expired (expires_at_ms={}, now={})",
                    snapshot_id, snap.expires_at_ms, now_ms
                ),
            ))));
        }

        // Validate database scope.
        let current_db_id = {
            let session = self.auth().session.lock().await;
            session.current_database_id()
        };
        if snap.database_id != current_db_id {
            return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                "ERROR".to_string(),
                "42501".to_string(),
                format!(
                    "snapshot '{}' belongs to database {}, current database is {}",
                    snapshot_id, snap.database_id, current_db_id
                ),
            ))));
        }

        // Get TiKV client.
        let tikv_client = self
            .auth()
            .executor
            .store()
            .transaction_client()
            .ok_or_else(|| {
                PgWireError::UserError(Box::new(ErrorInfo::new(
                    "ERROR".to_string(),
                    "XX000".to_string(),
                    "TiKV client not available".to_string(),
                )))
            })?;

        let snapshot_ts = snap.snapshot_ts;
        let db_id = snap.database_id;

        // Resolve bare table name to schema-qualified name. Storage uses
        // "public.<table>" for tables in the default schema.
        let qualified_name = if table_name.contains('.') {
            table_name.clone()
        } else {
            format!("public.{}", table_name)
        };

        // Look up table schema at the pinned snapshot.
        let schema =
            export::scan::get_schema_at_snapshot(&tikv_client, snapshot_ts, db_id, &qualified_name)
                .await
                .map_err(|e| {
                    PgWireError::UserError(Box::new(ErrorInfo::new(
                        "ERROR".to_string(),
                        "42P01".to_string(),
                        format!("failed to get schema for '{}': {e}", qualified_name),
                    )))
                })?;

        let col_count = schema.columns.len();
        let table_id = schema.table_id;

        // Send CopyOutResponse header.
        let column_formats: Vec<i16> = vec![0; col_count]; // all text
        let copy_resp = CopyResponse::new(0, col_count, column_formats);
        pgwire::api::copy::send_copy_out_response(client, copy_resp).await?;

        // Default COPY options (TEXT format, tab delimiter).
        let copy_opts = CopyOptions::default();

        // Stream table data using the same pagination + byte-cap accumulation
        // as `export_table_scan`. We replicate the loop here because Rust's
        // async closures cannot borrow `client` (a `&mut Sink`) across await
        // points in an `FnMut` callback. The byte-cap logic is identical:
        // accumulate rows across TiKV pages, flush when cumulative value bytes
        // reach EXPORT_BATCH_BYTE_CAP or the scan is exhausted.
        let (raw_start, raw_end) = crate::storage::encode_table_data_range_v2(db_id, table_id);
        let mut cursor = raw_start;
        let mut total_rows: u64 = 0;
        let mut buf = Vec::with_capacity(4096);

        // Accumulate rows across TiKV pages until byte cap is reached.
        let mut batch_rows: Vec<crate::model::Row> = Vec::new();
        let mut batch_bytes: usize = 0;

        loop {
            let page =
                export::scan::scan_table_page(&tikv_client, snapshot_ts, cursor, raw_end.clone())
                    .await
                    .map_err(|e| {
                        PgWireError::UserError(Box::new(ErrorInfo::new(
                            "ERROR".to_string(),
                            "XX000".to_string(),
                            format!("export scan page failed: {e}"),
                        )))
                    })?;

            let page_exhausted = page.next_cursor.is_none();

            if !page.rows.is_empty() {
                batch_bytes += page.value_bytes;
                batch_rows.extend(page.rows);
            }

            // Flush batch if byte cap reached or scan exhausted.
            if !batch_rows.is_empty()
                && (batch_bytes >= export::scan::EXPORT_BATCH_BYTE_CAP || page_exhausted)
            {
                for row in &batch_rows {
                    buf.clear();
                    copy_format::encode_row_with_options(&row.values, &mut buf, &copy_opts)
                        .map_err(|e| {
                            PgWireError::UserError(Box::new(ErrorInfo::new(
                                "ERROR".to_string(),
                                "XX000".to_string(),
                                format!("encode row: {e}"),
                            )))
                        })?;
                    let data =
                        pgwire::messages::copy::CopyData::new(bytes::Bytes::copy_from_slice(&buf));
                    client.send(PgWireBackendMessage::CopyData(data)).await?;
                }
                total_rows += batch_rows.len() as u64;
                batch_rows.clear();
                batch_bytes = 0;
            }

            match page.next_cursor {
                Some(next) => cursor = next,
                None => break,
            }
        }

        // Send CopyDone + CommandComplete.
        let done = pgwire::messages::copy::CopyDone::new();
        client.send(PgWireBackendMessage::CopyDone(done)).await?;

        let complete =
            pgwire::messages::response::CommandComplete::new(format!("COPY {}", total_rows));
        client
            .send(PgWireBackendMessage::CommandComplete(complete))
            .await?;

        debug!(
            table_name,
            snapshot_id, total_rows, "export snapshot copy complete"
        );

        Ok(Some(vec![]))
    }
}

// ---------------------------------------------------------------------------
// Prefix matching (extracted so unit tests can exercise the production path)
// ---------------------------------------------------------------------------

const EXPORT_COPY_PREFIX: &str = "EXPORT SNAPSHOT COPY ";

/// If `query` starts with `EXPORT SNAPSHOT COPY ` (case-insensitive, after
/// trimming), return the remainder. Uses byte-level comparison to avoid
/// panicking on multi-byte UTF-8 input (see #2357).
fn strip_export_copy_prefix(query: &str) -> Option<&str> {
    let trimmed = query.trim();
    if trimmed.len() < EXPORT_COPY_PREFIX.len()
        || !trimmed.as_bytes()[..EXPORT_COPY_PREFIX.len()]
            .eq_ignore_ascii_case(EXPORT_COPY_PREFIX.as_bytes())
    {
        return None;
    }
    Some(trimmed[EXPORT_COPY_PREFIX.len()..].trim())
}

// ---------------------------------------------------------------------------
// Argument parsing
// ---------------------------------------------------------------------------

/// Parse `'<table_name>' '<snapshot_id>'` from the remainder after
/// `EXPORT SNAPSHOT COPY`.
fn parse_export_copy_args(s: &str) -> Result<(String, String), String> {
    let s = s.trim().trim_end_matches(';').trim();

    let table_name = parse_quoted(s).ok_or("expected quoted table_name")?;

    let after_table = skip_quoted(s).ok_or("expected quoted table_name")?.trim();

    let snapshot_id =
        parse_quoted(after_table).ok_or("expected quoted snapshot_id after table_name")?;

    Ok((table_name, snapshot_id))
}

/// Parse a single-quoted string: `'value'` → `value`.
fn parse_quoted(s: &str) -> Option<String> {
    let s = s.trim();
    if !s.starts_with('\'') {
        return None;
    }
    let inner = &s[1..];
    let mut result = String::new();
    let mut chars = inner.chars();
    while let Some(ch) = chars.next() {
        if ch == '\'' {
            if chars.clone().next() == Some('\'') {
                result.push('\'');
                chars.next();
            } else {
                return Some(result);
            }
        } else {
            result.push(ch);
        }
    }
    None
}

/// Skip past a single-quoted string, returning the remainder.
fn skip_quoted(s: &str) -> Option<&str> {
    let s = s.trim();
    if !s.starts_with('\'') {
        return None;
    }
    let inner = &s[1..];
    let mut chars = inner.char_indices();
    while let Some((i, ch)) = chars.next() {
        if ch == '\'' {
            if inner.get(i + 1..i + 2) == Some("'") {
                chars.next();
            } else {
                return Some(&inner[i + 1..]);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_export_copy_args_basic() {
        let (table, snap) = parse_export_copy_args("'users' 'snap-123'").unwrap();
        assert_eq!(table, "users");
        assert_eq!(snap, "snap-123");
    }

    #[test]
    fn parse_export_copy_args_with_semicolon() {
        let (table, snap) = parse_export_copy_args("'orders' 'snap-456';").unwrap();
        assert_eq!(table, "orders");
        assert_eq!(snap, "snap-456");
    }

    #[test]
    fn parse_export_copy_args_missing_snapshot() {
        assert!(parse_export_copy_args("'users'").is_err());
    }

    #[test]
    fn parse_export_copy_args_escaped_quotes() {
        let (table, snap) = parse_export_copy_args("'my''table' 'snap''id'").unwrap();
        assert_eq!(table, "my'table");
        assert_eq!(snap, "snap'id");
    }

    /// Regression test for #2357: `strip_export_copy_prefix` (the production
    /// prefix check used by `try_handle_export_snapshot_copy`) must not panic
    /// when the query contains multi-byte UTF-8 characters.
    #[test]
    fn strip_export_copy_prefix_does_not_panic_on_multibyte_utf8() {
        // "SELECT * FROM 日本語テスト" — byte 21 (EXPORT_COPY_PREFIX.len())
        // lands inside the 3-byte CJK char 語. Old str[..21] would panic.
        assert_eq!(strip_export_copy_prefix("SELECT * FROM 日本語テスト"), None);

        // Pure CJK, short, empty — must not panic, must return None.
        assert_eq!(strip_export_copy_prefix("你好世界"), None);
        assert_eq!(strip_export_copy_prefix("EXPORT SNAPSHOT 测试"), None);
        assert_eq!(strip_export_copy_prefix(""), None);
        assert_eq!(strip_export_copy_prefix("E"), None);

        // Valid prefix — must return the remainder.
        assert_eq!(
            strip_export_copy_prefix("EXPORT SNAPSHOT COPY 'tbl' 'snap'"),
            Some("'tbl' 'snap'")
        );
        // Case-insensitive.
        assert_eq!(
            strip_export_copy_prefix("export snapshot copy 'tbl' 'snap'"),
            Some("'tbl' 'snap'")
        );
        // Leading/trailing whitespace.
        assert_eq!(
            strip_export_copy_prefix("  Export Snapshot Copy  'tbl' 'snap'  "),
            Some("'tbl' 'snap'")
        );
        // Multi-byte AFTER the prefix — must still work.
        assert_eq!(
            strip_export_copy_prefix("EXPORT SNAPSHOT COPY '日本語テーブル' 'snap'"),
            Some("'日本語テーブル' 'snap'")
        );
    }
}
