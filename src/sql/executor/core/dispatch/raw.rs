//! Raw-SQL dispatch: routes statements classified by [`RawSqlKind`] before
//! `sqlparser` parsing.
//!
//! Two clearly separated paths:
//!
//! 1. **Instrumented** ([`Executor::try_dispatch_raw_instrumented`]) — uses
//!    the same record + mark-failed contract as the former `dispatch_raw!`
//!    macro. Only entered for non-observability users.
//!
//! 2. **Passthrough** ([`Executor::try_dispatch_raw_passthrough`]) — handles
//!    `ALTER SYSTEM SET` and `RESET` outside instrumented dispatch, preserving
//!    invariant I2: no raw observability record, no raw
//!    auto-`mark_transaction_failed`.

use super::super::*;
use super::scaffold::DispatchContext;
use crate::sql::raw_sql::RawSqlKind;

/// Strip all leading SQL comments (`/* ... */` and `-- ...\n`) from a string,
/// returning the remaining trimmed slice. Used by the LISTEN/NOTIFY/UNLISTEN
/// stub validators so that comments between the keyword and identifier are
/// accepted (PostgreSQL permits this).
fn strip_sql_comments(s: &str) -> &str {
    let mut s = s.trim();
    loop {
        if s.starts_with("/*") {
            match s.find("*/") {
                Some(end) => {
                    s = s[end + 2..].trim();
                    continue;
                }
                None => break, // unclosed comment
            }
        } else if s.starts_with("--") {
            match s.find('\n') {
                Some(end) => {
                    s = s[end + 1..].trim();
                    continue;
                }
                None => {
                    s = "";
                    break;
                } // comment to end of string
            }
        } else {
            break;
        }
    }
    s
}

/// Consume a single-quoted string body (after the opening `'`).
/// Returns `Some(remainder)` with the slice after the closing quote,
/// or `None` if the string is unterminated. Handles `''` escapes.
/// When `escape_backslash` is true (E-strings), `\` followed by any
/// character is treated as an escape sequence.
fn consume_single_quoted(s: &str, escape_backslash: bool) -> Option<&str> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if escape_backslash && bytes[i] == b'\\' {
            i += 2; // backslash escape: skip \ and the following char
        } else if bytes[i] == b'\'' {
            if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                i += 2; // escaped quote ''
            } else {
                return Some(&s[i + 1..]);
            }
        } else {
            i += 1;
        }
    }
    None // unterminated
}

/// Consume a complete PostgreSQL string literal starting at the beginning of
/// `s` (after trimming whitespace). Returns `Some(remainder)` with the slice
/// after the closing delimiter, or `None` if the literal is unterminated or
/// not a recognised form.
///
/// Supported forms: `'...'`, `E'...'`/`e'...'`, `B'...'`/`b'...'`,
/// `X'...'`/`x'...'`, `U&'...'`/`u&'...'`, `$$...$$`, `$tag$...$tag$`.
fn consume_pg_string_literal(s: &str) -> Option<&str> {
    let s = s.trim();
    if let Some(inner) = s.strip_prefix('\'') {
        consume_single_quoted(inner, false)
    } else if let Some(inner) = s.strip_prefix("E'").or_else(|| s.strip_prefix("e'")) {
        consume_single_quoted(inner, true)
    } else if let Some(inner) = s
        .strip_prefix("B'")
        .or_else(|| s.strip_prefix("b'"))
        .or_else(|| s.strip_prefix("X'"))
        .or_else(|| s.strip_prefix("x'"))
    {
        consume_single_quoted(inner, false)
    } else if let Some(inner) = s.strip_prefix("U&'").or_else(|| s.strip_prefix("u&'")) {
        consume_single_quoted(inner, false)
    } else if let Some(after_dollar) = s.strip_prefix('$') {
        // Dollar-quoted: find the delimiter tag
        let tag_end = after_dollar.find('$')?;
        let delimiter = &s[..tag_end + 2]; // e.g., "$$" or "$tag$"
        let body = &s[delimiter.len()..];
        let close_pos = body.find(delimiter)?;
        Some(&body[close_pos + delimiter.len()..])
    } else {
        None
    }
}

impl Executor {
    /// Attempt to dispatch the statement through instrumented raw-SQL paths.
    ///
    /// Returns `Some(result)` if the statement was handled (caller should
    /// return).  Returns `None` if no raw handler matched (caller should
    /// continue to parse/AST).
    ///
    /// Only entered for non-observability users.  The helpers
    /// `finish_raw_single` / `finish_raw_multi` record observability and mark
    /// transaction failure on error.
    ///
    /// Preserves the original two-block ordering:
    ///
    /// 1. First raw block (database/extension/comment/function/trigger ops)
    /// 2. Skip-reason check
    /// 3. Second raw block (alter/sequence/materialized-view/call/procedure/type/collation/analyze)
    pub(super) async fn try_dispatch_raw_instrumented(
        &self,
        session: &mut Session,
        sql: &str,
        ctx: &DispatchContext,
    ) -> Option<Result<ExecuteResults>> {
        debug_assert!(!ctx.is_observability_user);

        // ── First raw block ──────────────────────────────────────────
        if let Some(result) = self.try_first_raw_block(session, sql, ctx).await {
            return Some(result);
        }

        // ── Skip-reason gate (between first and second raw blocks) ──
        if let Some(reason) = get_skip_reason(&ctx.sql_upper) {
            return Some(Err(SqlError::Unsupported(reason).into()));
        }

        // ── Second raw block ─────────────────────────────────────────
        self.try_second_raw_block(session, sql, ctx).await
    }

    /// First raw block: database, extension, comment, function, trigger ops.
    async fn try_first_raw_block(
        &self,
        session: &mut Session,
        sql: &str,
        ctx: &DispatchContext,
    ) -> Option<Result<ExecuteResults>> {
        let kind = ctx.raw_kind?;
        match kind {
            RawSqlKind::CreateDatabase => {
                let start = Instant::now();
                let res = self.execute_create_database_cmd(session, sql).await;
                Some(self.finish_raw_multi(session, &ctx.sql_trimmed, start, res))
            }
            RawSqlKind::DropDatabase => {
                let start = Instant::now();
                let res = self.execute_drop_database_cmd(session, sql).await;
                Some(self.finish_raw_multi(session, &ctx.sql_trimmed, start, res))
            }
            RawSqlKind::AlterDatabase => {
                let start = Instant::now();
                let res = self.execute_alter_database_cmd(session, sql).await;
                Some(self.finish_raw_multi(session, &ctx.sql_trimmed, start, res))
            }
            RawSqlKind::CreateExtension => {
                let start = Instant::now();
                let res = self.execute_create_extension_cmd(session, sql).await;
                Some(self.finish_raw_single(session, &ctx.sql_trimmed, start, res))
            }
            RawSqlKind::DropExtension => {
                let start = Instant::now();
                let res = self.execute_drop_extension_cmd(session, sql).await;
                Some(self.finish_raw_single(session, &ctx.sql_trimmed, start, res))
            }
            RawSqlKind::CommentOn => {
                let start = Instant::now();
                let res = self.execute_comment_on_cmd(session, sql).await;
                Some(self.finish_raw_single(session, &ctx.sql_trimmed, start, res))
            }
            RawSqlKind::CreateFunction => {
                let start = Instant::now();
                let res = self.execute_create_function_cmd(session, sql).await;
                Some(self.finish_raw_single(session, &ctx.sql_trimmed, start, res))
            }
            RawSqlKind::DropFunction => {
                let start = Instant::now();
                let res = self.execute_drop_function_cmd(session, sql).await;
                Some(self.finish_raw_single(session, &ctx.sql_trimmed, start, res))
            }
            RawSqlKind::CreateTrigger => {
                let start = Instant::now();
                let res = self.execute_create_trigger_cmd(session, sql).await;
                Some(self.finish_raw_single(session, &ctx.sql_trimmed, start, res))
            }
            RawSqlKind::DropTrigger => {
                let start = Instant::now();
                let res = self.execute_drop_trigger_cmd(session, sql).await;
                Some(self.finish_raw_single(session, &ctx.sql_trimmed, start, res))
            }
            RawSqlKind::CreatePolicy => {
                let start = Instant::now();
                let res = self.execute_create_policy_cmd(session, sql).await;
                Some(self.finish_raw_single(session, &ctx.sql_trimmed, start, res))
            }
            RawSqlKind::AlterPolicy => {
                let start = Instant::now();
                let res = self.execute_alter_policy_cmd(session, sql).await;
                Some(self.finish_raw_single(session, &ctx.sql_trimmed, start, res))
            }
            RawSqlKind::DropPolicy => {
                let start = Instant::now();
                let res = self.execute_drop_policy_cmd(session, sql).await;
                Some(self.finish_raw_single(session, &ctx.sql_trimmed, start, res))
            }
            RawSqlKind::AlterTableRls => {
                let start = Instant::now();
                let res = self.execute_alter_table_rls_cmd(session, sql).await;
                Some(self.finish_raw_single(session, &ctx.sql_trimmed, start, res))
            }
            RawSqlKind::Listen => {
                let start = Instant::now();
                // LISTEN requires a channel name: at least one non-whitespace
                // token after the keyword.
                let rest = ctx.sql_trimmed.get(6..).unwrap_or("").trim();
                let rest = rest.trim_end_matches(';').trim();
                // Strip all leading comments (block and line)
                let rest = strip_sql_comments(rest);
                // Consume one identifier token and reject trailing junk
                let first_token_end = if let Some(inner) = rest.strip_prefix('"') {
                    // Quoted identifier — find closing quote, handling "" escapes
                    let mut i = 0;
                    let bytes = inner.as_bytes();
                    loop {
                        if i >= bytes.len() {
                            break 0; // unclosed quote
                        }
                        if bytes[i] == b'"' {
                            if i + 1 < bytes.len() && bytes[i + 1] == b'"' {
                                i += 2; // skip escaped ""
                            } else {
                                break i + 2; // include both quotes
                            }
                        } else {
                            i += 1;
                        }
                    }
                } else {
                    // Unquoted — alphanumeric + underscore + $
                    rest.find(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '$')
                        .unwrap_or(rest.len())
                };
                let after_token =
                    strip_sql_comments(rest[first_token_end..].trim().trim_end_matches(';').trim());
                let is_valid = first_token_end > 0
                    && rest.chars().next().is_some_and(|c| {
                        c.is_ascii_alphabetic() || c == '_' || c == '"' || c == '$'
                    })
                    && after_token.is_empty();
                let res: Result<ExecuteResult> = if !is_valid {
                    Err(crate::sql::error::SqlError::Syntax(
                        "syntax error at or near \"LISTEN\"".to_string(),
                    )
                    .into())
                } else {
                    Ok(ExecuteResult::CommandComplete { tag: "LISTEN" })
                };
                Some(self.finish_raw_single(session, &ctx.sql_trimmed, start, res))
            }
            RawSqlKind::Notify => {
                let start = Instant::now();
                // NOTIFY requires a channel name: at least one non-whitespace
                // token after the keyword (optional payload is allowed).
                let rest = ctx.sql_trimmed.get(6..).unwrap_or("").trim();
                let rest = rest.trim_end_matches(';').trim();
                // Strip all leading comments (block and line)
                let rest = strip_sql_comments(rest);
                // Consume one identifier token and reject trailing junk
                // (optional `, 'payload'` is allowed after the channel)
                let first_token_end = if let Some(inner) = rest.strip_prefix('"') {
                    // Quoted identifier — find closing quote, handling "" escapes
                    let mut i = 0;
                    let bytes = inner.as_bytes();
                    loop {
                        if i >= bytes.len() {
                            break 0; // unclosed quote
                        }
                        if bytes[i] == b'"' {
                            if i + 1 < bytes.len() && bytes[i + 1] == b'"' {
                                i += 2; // skip escaped ""
                            } else {
                                break i + 2; // include both quotes
                            }
                        } else {
                            i += 1;
                        }
                    }
                } else {
                    // Unquoted — alphanumeric + underscore + $
                    rest.find(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '$')
                        .unwrap_or(rest.len())
                };
                let after_token =
                    strip_sql_comments(rest[first_token_end..].trim().trim_end_matches(';').trim());
                let first_char_valid = rest
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_ascii_alphabetic() || c == '_' || c == '"' || c == '$');
                let is_valid = first_token_end > 0
                    && first_char_valid
                    && if after_token.is_empty() {
                        true // NOTIFY channel; — valid
                    } else if let Some(after_comma_raw) = after_token.strip_prefix(',') {
                        // After the comma, consume a complete PG string literal
                        // and verify it is properly terminated.
                        let after_comma = strip_sql_comments(after_comma_raw.trim());
                        match consume_pg_string_literal(after_comma) {
                            Some(rest) => {
                                let rest =
                                    strip_sql_comments(rest.trim().trim_end_matches(';').trim());
                                rest.is_empty()
                            }
                            None => false, // unterminated or missing payload
                        }
                    } else {
                        false // trailing junk
                    };
                let res: Result<ExecuteResult> = if !is_valid {
                    Err(crate::sql::error::SqlError::Syntax(
                        "syntax error at or near \"NOTIFY\"".to_string(),
                    )
                    .into())
                } else {
                    Ok(ExecuteResult::CommandComplete { tag: "NOTIFY" })
                };
                Some(self.finish_raw_single(session, &ctx.sql_trimmed, start, res))
            }
            RawSqlKind::Unlisten => {
                let start = Instant::now();
                // UNLISTEN requires a channel name or `*`: at least one
                // non-whitespace token after the keyword.
                let rest = ctx.sql_trimmed.get(8..).unwrap_or("").trim();
                let rest = rest.trim_end_matches(';').trim();
                // Strip all leading comments (block and line)
                let rest = strip_sql_comments(rest);
                // UNLISTEN accepts either * or an identifier, reject trailing junk
                let (is_valid, _) = if let Some(after_star_raw) = rest.strip_prefix('*') {
                    let after_star =
                        strip_sql_comments(after_star_raw.trim().trim_end_matches(';').trim());
                    (after_star.is_empty(), 1usize)
                } else {
                    let first_token_end = if let Some(inner) = rest.strip_prefix('"') {
                        // Quoted identifier — find closing quote, handling "" escapes
                        let mut i = 0;
                        let bytes = inner.as_bytes();
                        loop {
                            if i >= bytes.len() {
                                break 0; // unclosed quote
                            }
                            if bytes[i] == b'"' {
                                if i + 1 < bytes.len() && bytes[i + 1] == b'"' {
                                    i += 2; // skip escaped ""
                                } else {
                                    break i + 2; // include both quotes
                                }
                            } else {
                                i += 1;
                            }
                        }
                    } else {
                        // Unquoted — alphanumeric + underscore + $
                        rest.find(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '$')
                            .unwrap_or(rest.len())
                    };
                    let after_token = strip_sql_comments(
                        rest[first_token_end..].trim().trim_end_matches(';').trim(),
                    );
                    (
                        first_token_end > 0
                            && rest.chars().next().is_some_and(|c| {
                                c.is_ascii_alphabetic()
                                    || c == '_'
                                    || c == '"'
                                    || c == '*'
                                    || c == '$'
                            })
                            && after_token.is_empty(),
                        first_token_end,
                    )
                };
                let res: Result<ExecuteResult> = if !is_valid {
                    Err(crate::sql::error::SqlError::Syntax(
                        "syntax error at or near \"UNLISTEN\"".to_string(),
                    )
                    .into())
                } else {
                    Ok(ExecuteResult::CommandComplete { tag: "UNLISTEN" })
                };
                Some(self.finish_raw_single(session, &ctx.sql_trimmed, start, res))
            }
            RawSqlKind::ExportSnapshotBegin => {
                let start = Instant::now();
                let res = self.execute_begin_export_snapshot_cmd(session, sql).await;
                Some(self.finish_raw_multi(session, &ctx.sql_trimmed, start, res))
            }
            RawSqlKind::ExportSnapshotRelease => {
                let start = Instant::now();
                let res = self.execute_release_export_snapshot_cmd(session, sql).await;
                Some(self.finish_raw_multi(session, &ctx.sql_trimmed, start, res))
            }
            RawSqlKind::ExportSnapshotList => {
                let start = Instant::now();
                let res = self.execute_list_export_snapshots_cmd(session, sql).await;
                Some(self.finish_raw_multi(session, &ctx.sql_trimmed, start, res))
            }
            _ => None,
        }
    }

    /// Second raw block: alter/sequence/materialized-view/call/procedure/type/collation/analyze.
    async fn try_second_raw_block(
        &self,
        session: &mut Session,
        sql: &str,
        ctx: &DispatchContext,
    ) -> Option<Result<ExecuteResults>> {
        let kind = ctx.raw_kind?;
        match kind {
            RawSqlKind::AlterOwnerTo => {
                let start = Instant::now();
                let res = self.execute_alter_owner_cmd(session, sql).await;
                Some(self.finish_raw_single(session, &ctx.sql_trimmed, start, res))
            }
            RawSqlKind::AlterDefaultPrivileges => {
                let start = Instant::now();
                let res = self
                    .execute_alter_default_privileges_cmd(session, sql)
                    .await;
                Some(self.finish_raw_single(session, &ctx.sql_trimmed, start, res))
            }
            RawSqlKind::AlterSequenceOwnedBy => {
                let start = Instant::now();
                let res = self.execute_alter_sequence_owned_by_cmd(session, sql).await;
                Some(self.finish_raw_single(session, &ctx.sql_trimmed, start, res))
            }
            RawSqlKind::RefreshMaterializedView => {
                let start = Instant::now();
                let res = self
                    .execute_refresh_materialized_view_cmd(session, sql)
                    .await;
                Some(self.finish_raw_single(session, &ctx.sql_trimmed, start, res))
            }
            RawSqlKind::DropMaterializedView => {
                let start = Instant::now();
                let res = self.execute_drop_materialized_view_cmd(session, sql).await;
                Some(self.finish_raw_single(session, &ctx.sql_trimmed, start, res))
            }
            RawSqlKind::Call => {
                let start = Instant::now();
                let res = self.execute_call_cmd(session, sql).await;
                Some(self.finish_raw_single(session, &ctx.sql_trimmed, start, res))
            }
            RawSqlKind::DropProcedure => {
                let start = Instant::now();
                let res = self.execute_drop_procedure_cmd(session, sql).await;
                Some(self.finish_raw_single(session, &ctx.sql_trimmed, start, res))
            }
            RawSqlKind::CreateProcedure => {
                let start = Instant::now();
                let res = self.execute_create_procedure_cmd(session, sql).await;
                Some(self.finish_raw_single(session, &ctx.sql_trimmed, start, res))
            }
            RawSqlKind::CreateTypeEnum => {
                let start = Instant::now();
                let res = self.execute_create_type_enum_cmd(session, sql).await;
                Some(self.finish_raw_single(session, &ctx.sql_trimmed, start, res))
            }
            RawSqlKind::AlterType => {
                let start = Instant::now();
                let res = self.execute_alter_type_cmd(session, sql).await;
                Some(self.finish_raw_multi(session, &ctx.sql_trimmed, start, res))
            }
            RawSqlKind::DropType => {
                let start = Instant::now();
                let res = self.execute_drop_type_cmd(session, sql).await;
                Some(self.finish_raw_single(session, &ctx.sql_trimmed, start, res))
            }
            RawSqlKind::CreateCollation => {
                let start = Instant::now();
                let res = self.execute_create_collation_cmd(session, sql).await;
                Some(self.finish_raw_single(session, &ctx.sql_trimmed, start, res))
            }
            RawSqlKind::DropCollation => {
                let start = Instant::now();
                let res = self.execute_drop_collation_cmd(session, sql).await;
                Some(self.finish_raw_single(session, &ctx.sql_trimmed, start, res))
            }
            RawSqlKind::Analyze => {
                let start = Instant::now();
                let res = self.execute_analyze_cmd(session, &ctx.sql_trimmed).await;
                Some(self.finish_raw_single(session, &ctx.sql_trimmed, start, res))
            }
            RawSqlKind::Do => {
                let start = Instant::now();
                let res = self.execute_do_block_cmd(session, sql).await;
                Some(self.finish_raw_single(session, &ctx.sql_trimmed, start, res))
            }
            RawSqlKind::AlterIndexIfExists => {
                let start = Instant::now();
                let res = self
                    .execute_alter_index_if_exists_rename(session, &ctx.sql_trimmed)
                    .await;
                Some(self.finish_raw_single(session, &ctx.sql_trimmed, start, res))
            }
            RawSqlKind::CreateTextSearchConfiguration => {
                let start = Instant::now();
                let res = self
                    .execute_create_text_search_configuration_cmd(session, sql)
                    .await;
                Some(self.finish_raw_single(session, &ctx.sql_trimmed, start, res))
            }
            RawSqlKind::DropTextSearchConfiguration => {
                let start = Instant::now();
                let res = self
                    .execute_drop_text_search_configuration_cmd(session, sql)
                    .await;
                Some(self.finish_raw_single(session, &ctx.sql_trimmed, start, res))
            }
            RawSqlKind::AlterTextSearchConfiguration => {
                let start = Instant::now();
                let res = self
                    .execute_alter_text_search_configuration_cmd(session, sql)
                    .await;
                Some(self.finish_raw_single(session, &ctx.sql_trimmed, start, res))
            }
            _ => None,
        }
    }

    /// Attempt to dispatch `ALTER SYSTEM SET` or `RESET` through the
    /// passthrough path that bypasses instrumented dispatch.
    ///
    /// **Invariant I2**: These branches produce no raw observability record
    /// and no raw auto-`mark_transaction_failed` on either success or error.
    ///
    /// Returns `Some(result)` if handled, `None` otherwise.
    pub(super) fn try_dispatch_raw_passthrough(
        &self,
        session: &mut Session,
        ctx: &DispatchContext,
    ) -> Option<Result<ExecuteResults>> {
        match ctx.raw_kind {
            Some(RawSqlKind::AlterSystemSet) => {
                Some(self.execute_alter_system_set(session, &ctx.sql_trimmed))
            }
            Some(RawSqlKind::Reset) => Some(Self::execute_reset(session, &ctx.sql_trimmed)),
            _ => None,
        }
    }

    // ── Instrumented raw helpers (equivalent to dispatch_raw! macro) ──

    /// Finish an instrumented raw dispatch for a command returning
    /// `ExecuteResult`.
    fn finish_raw_single(
        &self,
        session: &mut Session,
        sql_obs: &str,
        start: Instant,
        res: Result<ExecuteResult>,
    ) -> Result<ExecuteResults> {
        if res.is_err() && session.is_in_transaction() {
            session.mark_transaction_failed();
        }
        self.observability
            .record_statement(start.elapsed(), res.is_ok(), || sql_obs.to_string());
        res.map(ExecuteResults::single)
    }

    /// Finish an instrumented raw dispatch for a command returning
    /// `ExecuteResults`.
    fn finish_raw_multi(
        &self,
        session: &mut Session,
        sql_obs: &str,
        start: Instant,
        res: Result<ExecuteResults>,
    ) -> Result<ExecuteResults> {
        if res.is_err() && session.is_in_transaction() {
            session.mark_transaction_failed();
        }
        self.observability
            .record_statement(start.elapsed(), res.is_ok(), || sql_obs.to_string());
        res
    }

    // ── Passthrough implementations (I2) ─────────────────────────────

    /// Execute `ALTER SYSTEM SET <guc> = <value>`.
    ///
    /// Passthrough: no instrumented dispatch, no observability record, no auto
    /// `mark_transaction_failed`.
    fn execute_alter_system_set(
        &self,
        session: &mut Session,
        sql_trimmed: &str,
    ) -> Result<ExecuteResults> {
        if !session.is_superuser() {
            return Err(SqlError::PermissionDenied {
                object_type: "system".to_string(),
                object_name: "ALTER SYSTEM SET".to_string(),
            }
            .into());
        }

        let rest = sql_trimmed.get(17..).unwrap_or("").trim();
        let rest_clean = rest.trim_end_matches(';').trim();
        let (name, raw_value) = if let Some(pos) = rest_clean.find('=') {
            (&rest_clean[..pos], &rest_clean[pos + 1..])
        } else {
            let rest_upper = rest_clean.to_ascii_uppercase();
            if let Some(pos) = rest_upper.find(" TO ") {
                (&rest_clean[..pos], &rest_clean[pos + 4..])
            } else {
                return Err(anyhow!("syntax error in ALTER SYSTEM SET"));
            }
        };

        let name_lower = name.trim().to_lowercase();
        let value_clean = raw_value.trim().trim_matches('\'').trim_matches('"').trim();

        match name_lower.as_str() {
            "statement_timeout" | "idle_in_transaction_session_timeout" => {}
            _ => {
                return Err(anyhow!(
                    "ALTER SYSTEM SET is only supported for statement_timeout and idle_in_transaction_session_timeout"
                ));
            }
        }

        let ms = crate::sql::session::SessionSettings::parse_timeout_value(value_clean)?;

        let server_config = session
            .server_config()
            .ok_or_else(|| anyhow!("server configuration not available"))?;

        {
            let mut cfg = server_config.write();
            match name_lower.as_str() {
                "statement_timeout" => cfg.statement_timeout_ms = ms,
                "idle_in_transaction_session_timeout" => {
                    cfg.idle_in_transaction_session_timeout_ms = ms
                }
                _ => unreachable!(),
            }
        }

        Ok(ExecuteResults::single(ExecuteResult::CommandComplete {
            tag: "ALTER SYSTEM",
        }))
    }

    /// Execute `RESET <guc>`, `RESET ALL`, or `RESET "quoted"`.
    ///
    /// Passthrough: no instrumented dispatch, no observability record, no auto
    /// `mark_transaction_failed`.
    fn execute_reset(session: &mut Session, sql_trimmed: &str) -> Result<ExecuteResults> {
        use crate::sql::error::SqlError;

        let after_kw = sql_trimmed.get(5..).unwrap_or("");

        // PG grammar: `RESET ALL` is a keyword production, `RESET var_name`
        // uses ColId.  Use the permissive parser to detect ALL first, then
        // the strict parser (with ColId keyword rejection) for var_name.
        let permissive = crate::sql::raw_sql::extract_reset_name(after_kw)
            .ok_or_else(|| SqlError::Syntax("syntax error at or near \"RESET\"".to_string()))?;

        if permissive.name == "all" && !permissive.first_quoted {
            // RESET ALL — reset all settings to defaults.
            session.reset_all_settings();
        } else if permissive.name == "all" && permissive.first_quoted {
            // RESET "ALL" — PG treats quoted ALL as a parameter name, not the
            // keyword.  No such parameter exists → 42704.
            // Preserve original quoted case in the error message.
            return Err(SqlError::UndefinedObject(format!(
                "unrecognized configuration parameter {}",
                permissive.original
            ))
            .into());
        } else {
            // For non-ALL names, apply strict ColId grammar with keyword rejection.
            let rn = crate::sql::raw_sql::parse_reset_var_name(after_kw)
                .ok_or_else(|| SqlError::Syntax("syntax error at or near \"RESET\"".to_string()))?;
            // Quoted "ROLE" must reset effective role state, not just the setting.
            if rn.name == "role" && rn.first_quoted {
                session.reset_role();
            } else {
                crate::sql::executor::check_reserved_guc_reset_with_original(
                    &rn.name,
                    &rn.original,
                )?;
                if let Some(err) =
                    crate::sql::session::settings::SessionSettings::rejected_public_guc_error(
                        &rn.name,
                    )
                {
                    return Err(err.into());
                }
                // PostgreSQL errors on unknown bare parameters (SQLSTATE 42704).
                // Unknown dotted names (custom GUC namespaces like `db9.foo`) are
                // still exempt, but explicitly rejected stale public names are
                // handled by the guard above.
                if !rn.name.contains('.') && session.show_setting_value(&rn.name).is_none() {
                    // Use original token text (case-preserved) for the error
                    // message, stripping surrounding SQL double-quotes so the
                    // message reads e.g. `"FOOBAR"` not `"foobar"` for quoted
                    // identifiers — matching PostgreSQL behavior.
                    let display = rn.original.trim_matches('"');
                    return Err(SqlError::UndefinedObject(format!(
                        "unrecognized configuration parameter \"{}\"",
                        display
                    ))
                    .into());
                }
                session.reset_setting(&rn.name);
            }
        }
        Ok(ExecuteResults::single(ExecuteResult::CommandComplete {
            tag: "RESET",
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ServerConfig;
    use crate::sql::executor::core::dispatch::scaffold::DispatchContext;

    fn make_executor_and_session(
        is_superuser: bool,
        with_server_config: bool,
    ) -> (Executor, Session) {
        let store = crate::storage::TikvStore::new_stub();
        let keyspace = format!(
            "raw_dispatch_tests_{}",
            if is_superuser { "su" } else { "nsu" }
        );
        let observability = crate::observability::registry().tenant(&keyspace);
        let trigger_cache = std::sync::Arc::new(crate::sql::triggers::TriggerBodyCache::new());
        let rls_policy_cache = std::sync::Arc::new(crate::sql::rls::cache::RlsPolicyCache::new());
        let stats_cache = std::sync::Arc::new(crate::sql::stats::TableStatsCache::new());
        let executor = Executor::new(
            store.clone(),
            keyspace,
            observability.clone(),
            crate::pool::TenantMemoryAccountant::unlimited("raw_dispatch_tests".to_string()),
            trigger_cache,
            rls_policy_cache,
            stats_cache,
        );
        let mut session = Session::new_with_user_and_database(
            store,
            observability,
            "tester".to_string(),
            is_superuser,
            false,
            1,
            1,
            "postgres".to_string(),
            0,
            0,
        )
        .unwrap();
        if with_server_config {
            session.set_server_config(ServerConfig::default().shared());
        }
        (executor, session)
    }

    fn assert_command_tag(results: ExecuteResults, expected_tag: &'static str) {
        match results.last() {
            ExecuteResult::CommandComplete { tag } => assert_eq!(tag, expected_tag),
            other => panic!("expected command complete, got {other:?}"),
        }
    }

    #[test]
    fn alter_system_set_requires_superuser() {
        let (executor, mut session) = make_executor_and_session(false, true);
        let err = executor
            .execute_alter_system_set(&mut session, "ALTER SYSTEM SET statement_timeout = '5s'")
            .unwrap_err()
            .to_string();
        assert!(err.contains("permission denied"));
    }

    #[test]
    fn alter_system_set_rejects_invalid_syntax_and_unknown_guc() {
        let (executor, mut session) = make_executor_and_session(true, true);
        let syntax_err = executor
            .execute_alter_system_set(&mut session, "ALTER SYSTEM SET")
            .unwrap_err()
            .to_string();
        assert!(syntax_err.contains("syntax error"));

        let unsupported_err = executor
            .execute_alter_system_set(&mut session, "ALTER SYSTEM SET work_mem = '4MB'")
            .unwrap_err()
            .to_string();
        assert!(unsupported_err.contains("only supported for"));
    }

    #[test]
    fn alter_system_set_updates_server_config_for_supported_gucs() {
        let (executor, mut session) = make_executor_and_session(true, true);

        let r1 = executor
            .execute_alter_system_set(
                &mut session,
                "ALTER SYSTEM SET statement_timeout = '1500ms'",
            )
            .unwrap();
        assert_command_tag(r1, "ALTER SYSTEM");

        let r2 = executor
            .execute_alter_system_set(
                &mut session,
                "ALTER SYSTEM SET idle_in_transaction_session_timeout TO '2s'",
            )
            .unwrap();
        assert_command_tag(r2, "ALTER SYSTEM");

        let cfg = session.server_config().unwrap().read().clone();
        assert_eq!(cfg.statement_timeout_ms, 1500);
        assert_eq!(cfg.idle_in_transaction_session_timeout_ms, 2000);
    }

    #[test]
    fn alter_system_set_requires_server_config_handle() {
        let (executor, mut session) = make_executor_and_session(true, false);
        let err = executor
            .execute_alter_system_set(&mut session, "ALTER SYSTEM SET statement_timeout = 100")
            .unwrap_err()
            .to_string();
        assert!(err.contains("server configuration not available"));
    }

    #[test]
    fn execute_reset_handles_single_and_all() {
        let (_, mut session) = make_executor_and_session(true, false);
        session
            .set_known_setting("statement_timeout", "2500".to_string())
            .unwrap();
        assert_eq!(
            session.show_setting_value("statement_timeout").as_deref(),
            Some("2500ms")
        );

        let single = Executor::execute_reset(&mut session, "RESET statement_timeout").unwrap();
        assert_command_tag(single, "RESET");
        assert_eq!(
            session.show_setting_value("statement_timeout").as_deref(),
            Some("0")
        );

        session
            .set_known_setting("statement_timeout", "3000".to_string())
            .unwrap();
        let all = Executor::execute_reset(&mut session, "RESET ALL").unwrap();
        assert_command_tag(all, "RESET");
    }

    #[test]
    fn execute_reset_rejects_invalid_syntax() {
        let (_, mut session) = make_executor_and_session(true, false);
        let err = Executor::execute_reset(&mut session, "RESET").unwrap_err();
        assert!(err.to_string().contains("syntax error"));
        let sql_err = err.downcast_ref::<crate::sql::error::SqlError>().unwrap();
        assert_eq!(sql_err.sqlstate(), "42601");
    }

    #[test]
    fn execute_reset_quoted_is_superuser() {
        let (_, mut session) = make_executor_and_session(true, false);
        let err = Executor::execute_reset(&mut session, "RESET \"is_superuser\"").unwrap_err();
        assert!(
            err.to_string()
                .contains("parameter \"is_superuser\" cannot be changed"),
            "unexpected: {}",
            err
        );
        let sql_err = err.downcast_ref::<crate::sql::error::SqlError>().unwrap();
        assert_eq!(sql_err.sqlstate(), "55P02");
    }

    #[test]
    fn execute_reset_quoted_is_superuser_preserves_case() {
        let (_, mut session) = make_executor_and_session(true, false);
        // Uppercase quoted form should preserve the original case in the error
        let err = Executor::execute_reset(&mut session, "RESET \"IS_SUPERUSER\"").unwrap_err();
        assert!(
            err.to_string()
                .contains("parameter \"IS_SUPERUSER\" cannot be changed"),
            "unexpected: {}",
            err
        );
    }

    #[test]
    fn execute_reset_quoted_all() {
        let (_, mut session) = make_executor_and_session(true, false);
        let err = Executor::execute_reset(&mut session, "RESET \"ALL\"").unwrap_err();
        assert!(
            err.to_string()
                .contains("unrecognized configuration parameter \"ALL\""),
            "unexpected: {}",
            err
        );
        let sql_err = err.downcast_ref::<crate::sql::error::SqlError>().unwrap();
        assert_eq!(sql_err.sqlstate(), "42704");
    }

    #[test]
    fn execute_reset_quoted_all_lowercase_preserves_case() {
        let (_, mut session) = make_executor_and_session(true, false);
        let err = Executor::execute_reset(&mut session, "RESET \"all\"").unwrap_err();
        // Original case "all" must appear in the error message
        assert!(
            err.to_string()
                .contains("unrecognized configuration parameter \"all\""),
            "unexpected: {}",
            err
        );
    }

    #[test]
    fn execute_reset_quoted_role_resets_role_state() {
        let (_, mut session) = make_executor_and_session(true, false);
        // RESET "ROLE" should succeed and reset role (not error as unknown param)
        let result = Executor::execute_reset(&mut session, "RESET \"ROLE\"");
        assert!(
            result.is_ok(),
            "RESET \"ROLE\" should succeed: {:?}",
            result
        );
        assert_command_tag(result.unwrap(), "RESET");
    }

    #[test]
    fn execute_reset_rejects_empty_quoted_identifier() {
        let (_, mut session) = make_executor_and_session(true, false);
        let err = Executor::execute_reset(&mut session, "RESET \"\"").unwrap_err();
        assert!(err.to_string().contains("syntax error"));
    }

    #[test]
    fn execute_reset_reserved_keyword_gives_syntax_error() {
        let (_, mut session) = make_executor_and_session(true, false);
        // DEFAULT is a reserved keyword, not a valid ColId
        let err = Executor::execute_reset(&mut session, "RESET DEFAULT").unwrap_err();
        assert!(err.to_string().contains("syntax error"));
        let sql_err = err.downcast_ref::<crate::sql::error::SqlError>().unwrap();
        assert_eq!(sql_err.sqlstate(), "42601");
    }

    #[test]
    fn execute_reset_dotted_quoted_name() {
        let (_, mut session) = make_executor_and_session(true, false);
        // RESET "session"."authorization" resets a custom GUC (not the pseudo-GUC)
        let result = Executor::execute_reset(&mut session, "RESET \"session\".\"authorization\"");
        assert!(result.is_ok());
    }

    /// `RESET "ALL"` (quoted) must NOT reset all settings — it must be treated
    /// as an unknown parameter name and error with SQLSTATE 42704 (#1662).
    #[test]
    fn execute_reset_quoted_all_is_not_keyword() {
        let (_, mut session) = make_executor_and_session(true, false);
        session
            .set_known_setting("statement_timeout", "5000".to_string())
            .unwrap();

        let err = Executor::execute_reset(&mut session, "RESET \"ALL\"").unwrap_err();
        let msg = err.to_string();
        // Must preserve quoted original case: "ALL" not "all" (#1662).
        assert!(
            msg.contains("unrecognized configuration parameter \"ALL\""),
            "expected case-preserved quoted error, got: {msg}"
        );

        // Verify settings were NOT reset (statement_timeout still modified).
        assert_eq!(
            session.show_setting_value("statement_timeout").as_deref(),
            Some("5000ms")
        );
    }

    /// `RESET unknown_param` must error with SQLSTATE 42704 (#1662).
    #[test]
    fn execute_reset_unknown_param_errors() {
        let (_, mut session) = make_executor_and_session(true, false);
        let err =
            Executor::execute_reset(&mut session, "RESET definitely_missing_setting").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("unrecognized configuration parameter \"definitely_missing_setting\""),
            "expected unrecognized parameter error, got: {msg}"
        );
        // Verify SQLSTATE is 42704
        let sql_err = err
            .downcast_ref::<crate::sql::error::SqlError>()
            .expect("must be SqlError");
        assert_eq!(sql_err.sqlstate(), "42704");
    }

    #[test]
    fn execute_reset_rejected_public_agg_pushdown_guc_errors() {
        let (_, mut session) = make_executor_and_session(true, false);
        let err =
            Executor::execute_reset(&mut session, "RESET db9.enable_cop_agg_pushdown").unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("unrecognized configuration parameter \"db9.enable_cop_agg_pushdown\""),
            "expected rejected public GUC error, got: {msg}"
        );
        assert!(
            msg.contains("db9.enable_cop_pushdown"),
            "expected one-switch hint, got: {msg}"
        );
        let sql_err = err
            .downcast_ref::<crate::sql::error::SqlError>()
            .expect("must be SqlError");
        assert_eq!(sql_err.sqlstate(), "42704");
    }

    #[test]
    fn execute_reset_session_replication_role_is_harmless_noop() {
        let (_, mut session) = make_executor_and_session(true, false);
        let result = Executor::execute_reset(&mut session, "RESET session_replication_role");
        assert!(
            result.is_ok(),
            "RESET session_replication_role should succeed"
        );
        assert_command_tag(result.unwrap(), "RESET");
        assert_eq!(
            session
                .show_setting_value("session_replication_role")
                .as_deref(),
            Some("origin")
        );
    }

    #[test]
    fn alter_system_set_rejects_invalid_timeout_literal() {
        let (executor, mut session) = make_executor_and_session(true, true);
        let err = executor
            .execute_alter_system_set(
                &mut session,
                "ALTER SYSTEM SET statement_timeout = 'not_a_timeout'",
            )
            .unwrap_err()
            .to_string();
        assert!(err.to_lowercase().contains("timeout"));
    }

    #[test]
    fn finish_raw_helpers_mark_failed_only_when_in_transaction() {
        let (executor, mut session) = make_executor_and_session(true, false);
        session.force_test_transaction_state(true, false);

        let err_single = executor.finish_raw_single(
            &mut session,
            "SELECT 1",
            Instant::now(),
            Err(anyhow!("boom")),
        );
        assert!(err_single.is_err());
        assert!(session.is_transaction_failed());

        session.force_test_transaction_state(false, false);
        let err_multi = executor.finish_raw_multi(
            &mut session,
            "SELECT 1",
            Instant::now(),
            Err(anyhow!("boom2")),
        );
        assert!(err_multi.is_err());
        assert!(!session.is_transaction_failed());
    }

    #[test]
    fn passthrough_dispatch_routes_reset_and_alter_system_set() {
        let (executor, mut session) = make_executor_and_session(true, true);

        let alter_ctx =
            DispatchContext::new("ALTER SYSTEM SET statement_timeout = '1200ms'", &session);
        let alter = executor
            .try_dispatch_raw_passthrough(&mut session, &alter_ctx)
            .expect("should dispatch alter system")
            .unwrap();
        assert_command_tag(alter, "ALTER SYSTEM");

        let reset_ctx = DispatchContext::new("RESET statement_timeout", &session);
        let reset = executor
            .try_dispatch_raw_passthrough(&mut session, &reset_ctx)
            .expect("should dispatch reset")
            .unwrap();
        assert_command_tag(reset, "RESET");

        let other_ctx = DispatchContext::new("SELECT 1", &session);
        assert!(executor
            .try_dispatch_raw_passthrough(&mut session, &other_ctx)
            .is_none());
    }
}
