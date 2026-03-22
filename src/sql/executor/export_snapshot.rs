//! Export snapshot SQL command handlers.
//!
//! Implements the raw-SQL dispatch for:
//! - `EXPORT SNAPSHOT BEGIN <ttl_secs> '<owner_ref>'`
//! - `EXPORT SNAPSHOT RELEASE '<snapshot_id>'`
//! - `EXPORT SNAPSHOT LIST`
//!
//! All commands require superuser privileges.
//! These map to the export snapshot registry (S0) and return results
//! via `ExecuteResult::Select` rows.

use anyhow::{anyhow, Result};

use crate::export;
use crate::model::{DataType, Row, Value};
use crate::session_context;
use crate::sql::result::{ExecuteResult, ExecuteResults};
use crate::sql::session::Session;

use super::Executor;

/// Gate: require superuser for all export snapshot commands.
fn require_superuser(session: &Session) -> Result<()> {
    if !session.is_superuser() {
        return Err(anyhow!(
            "permission denied: EXPORT SNAPSHOT commands require superuser"
        ));
    }
    Ok(())
}

impl Executor {
    /// `EXPORT SNAPSHOT BEGIN <ttl_secs> '<owner_ref>'`
    ///
    /// Creates an export snapshot for the current session's database.
    /// Returns: snapshot_id, snapshot_ts, expires_at_ms, database_size_estimate
    pub(crate) async fn execute_begin_export_snapshot_cmd(
        &self,
        session: &mut Session,
        sql: &str,
    ) -> Result<ExecuteResults> {
        require_superuser(session)?;

        let registry = export::global_registry()
            .ok_or_else(|| anyhow!("export snapshot subsystem not initialized"))?;

        let db_id = session.current_database_id();
        let keyspace = self.tenant_keyspace().to_owned();

        // Parse: EXPORT SNAPSHOT BEGIN <ttl_secs> '<owner_ref>'
        let sql_upper = sql.trim().to_uppercase();
        let rest = sql_upper
            .strip_prefix("EXPORT SNAPSHOT BEGIN")
            .ok_or_else(|| anyhow!("invalid EXPORT SNAPSHOT BEGIN syntax"))?
            .trim();

        // Re-parse from original SQL to preserve owner_ref casing.
        let original_rest = &sql.trim()[sql.trim().len() - rest.len()..];
        let (ttl_str, owner_ref) = parse_begin_args(original_rest)?;
        let ttl_secs: u64 = ttl_str
            .parse()
            .map_err(|_| anyhow!("invalid ttl_secs: '{}'", ttl_str))?;

        let result = registry
            .begin_export_snapshot(db_id, &keyspace, ttl_secs, &owner_ref)
            .await
            .map_err(|e| anyhow!("begin_export_snapshot failed: {e}"))?;

        Ok(ExecuteResults(vec![ExecuteResult::Select {
            columns: vec![
                "snapshot_id".to_owned(),
                "snapshot_ts".to_owned(),
                "expires_at_ms".to_owned(),
                "database_size_estimate".to_owned(),
            ],
            column_types: Some(vec![
                DataType::Text,
                DataType::Int64,
                DataType::Int64,
                DataType::Int64,
            ]),
            rows: vec![Row::new(vec![
                Value::Text(result.snapshot_id),
                Value::Int64(result.snapshot_ts as i64),
                Value::Int64(result.expires_at_ms),
                Value::Int64(result.database_size_estimate as i64),
            ])],
            timezone: session_context::current_timezone(),
        }]))
    }

    /// `EXPORT SNAPSHOT RELEASE '<snapshot_id>'`
    ///
    /// Releases an export snapshot. Idempotent. Requires superuser.
    pub(crate) async fn execute_release_export_snapshot_cmd(
        &self,
        session: &mut Session,
        sql: &str,
    ) -> Result<ExecuteResults> {
        require_superuser(session)?;

        let registry = export::global_registry()
            .ok_or_else(|| anyhow!("export snapshot subsystem not initialized"))?;

        // Case-insensitive prefix match, preserve original for quoted string parsing.
        let prefix_len = "EXPORT SNAPSHOT RELEASE".len();
        let sql_trimmed = sql.trim();
        if sql_trimmed.len() < prefix_len
            || !sql_trimmed[..prefix_len].eq_ignore_ascii_case("EXPORT SNAPSHOT RELEASE")
        {
            return Err(anyhow!("invalid EXPORT SNAPSHOT RELEASE syntax"));
        }
        let rest = sql_trimmed[prefix_len..].trim();

        let snapshot_id =
            parse_quoted_string(rest).ok_or_else(|| anyhow!("expected quoted snapshot_id"))?;

        registry
            .release_export_snapshot(&snapshot_id)
            .await
            .map_err(|e| anyhow!("release_export_snapshot failed: {e}"))?;

        Ok(ExecuteResults(vec![ExecuteResult::CommandComplete {
            tag: "RELEASE EXPORT SNAPSHOT",
        }]))
    }

    /// `EXPORT SNAPSHOT LIST`
    ///
    /// Lists export snapshots scoped to the current session's keyspace and database.
    /// Requires superuser.
    pub(crate) async fn execute_list_export_snapshots_cmd(
        &self,
        session: &mut Session,
        _sql: &str,
    ) -> Result<ExecuteResults> {
        require_superuser(session)?;

        let registry = export::global_registry()
            .ok_or_else(|| anyhow!("export snapshot subsystem not initialized"))?;

        let current_keyspace = self.tenant_keyspace();
        let current_db_id = session.current_database_id();

        let snapshots = registry.list_export_snapshots().await?;

        // Scope: only show snapshots belonging to current keyspace + database.
        let rows: Vec<Row> = snapshots
            .into_iter()
            .filter(|s| s.keyspace == current_keyspace && s.database_id == current_db_id)
            .map(|s| {
                Row::new(vec![
                    Value::Text(s.snapshot_id),
                    Value::Int64(s.database_id as i64),
                    Value::Text(s.keyspace),
                    Value::Int64(s.snapshot_ts as i64),
                    Value::Text(s.owner_ref),
                    Value::Text(s.state.to_string()),
                    Value::Int64(s.created_at_ms),
                    Value::Int64(s.expires_at_ms),
                    Value::Int64(s.database_size_estimate as i64),
                ])
            })
            .collect();

        Ok(ExecuteResults(vec![ExecuteResult::Select {
            columns: vec![
                "snapshot_id".to_owned(),
                "database_id".to_owned(),
                "keyspace".to_owned(),
                "snapshot_ts".to_owned(),
                "owner_ref".to_owned(),
                "state".to_owned(),
                "created_at_ms".to_owned(),
                "expires_at_ms".to_owned(),
                "database_size_estimate".to_owned(),
            ],
            column_types: Some(vec![
                DataType::Text,
                DataType::Int64,
                DataType::Text,
                DataType::Int64,
                DataType::Text,
                DataType::Text,
                DataType::Int64,
                DataType::Int64,
                DataType::Int64,
            ]),
            rows,
            timezone: session_context::current_timezone(),
        }]))
    }
}

// ---------------------------------------------------------------------------
// Argument parsing helpers
// ---------------------------------------------------------------------------

/// Parse `<ttl_secs> '<owner_ref>'` from the remainder after `EXPORT SNAPSHOT BEGIN`.
fn parse_begin_args(s: &str) -> Result<(String, String)> {
    let s = s.trim().trim_end_matches(';').trim();

    // Find first whitespace to split ttl_secs from owner_ref
    let space_pos = s
        .find(|c: char| c.is_whitespace())
        .ok_or_else(|| anyhow!("expected: <ttl_secs> '<owner_ref>'"))?;

    let ttl_str = s[..space_pos].trim().to_owned();
    let owner_part = s[space_pos..].trim();

    let owner_ref = parse_quoted_string(owner_part)
        .ok_or_else(|| anyhow!("expected quoted owner_ref, got: '{}'", owner_part))?;

    Ok((ttl_str, owner_ref))
}

/// Parse a single-quoted SQL string: `'value'` → `value`.
/// Handles escaped single quotes (`''` → `'`).
fn parse_quoted_string(s: &str) -> Option<String> {
    let s = s.trim().trim_end_matches(';').trim();
    if !s.starts_with('\'') {
        return None;
    }
    let inner = &s[1..];
    let mut result = String::new();
    let mut chars = inner.chars();
    while let Some(ch) = chars.next() {
        if ch == '\'' {
            // Check for escaped quote
            if chars.clone().next() == Some('\'') {
                result.push('\'');
                chars.next(); // consume the second quote
            } else {
                return Some(result);
            }
        } else {
            result.push(ch);
        }
    }
    None // unterminated
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_begin_args_basic() {
        let (ttl, owner) = parse_begin_args("3600 'backup:job-42'").unwrap();
        assert_eq!(ttl, "3600");
        assert_eq!(owner, "backup:job-42");
    }

    #[test]
    fn parse_begin_args_with_semicolon() {
        let (ttl, owner) = parse_begin_args("7200 'backup:test';").unwrap();
        assert_eq!(ttl, "7200");
        assert_eq!(owner, "backup:test");
    }

    #[test]
    fn parse_quoted_string_basic() {
        assert_eq!(parse_quoted_string("'hello'"), Some("hello".to_owned()));
    }

    #[test]
    fn parse_quoted_string_escaped() {
        assert_eq!(parse_quoted_string("'it''s'"), Some("it's".to_owned()));
    }

    #[test]
    fn parse_quoted_string_with_trailing() {
        assert_eq!(
            parse_quoted_string("'snap-id';"),
            Some("snap-id".to_owned())
        );
    }

    #[test]
    fn parse_quoted_string_unterminated() {
        assert_eq!(parse_quoted_string("'unterminated"), None);
    }

    #[test]
    fn parse_quoted_string_not_quoted() {
        assert_eq!(parse_quoted_string("unquoted"), None);
    }
}
