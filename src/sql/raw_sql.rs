//! Classification of statements that may bypass `sqlparser` parsing.
//!
//! tipg historically allowed a small set of PostgreSQL statements to reach the
//! executor even when `sqlparser` couldn't parse them. These statements are
//! either:
//! - handled by raw-SQL interception in the executor/protocol layer, or
//! - intentionally rejected with a deterministic "unsupported" reason.
//!
//! This module centralizes the statement-prefix classification so protocol
//! parsing and executor dispatch stay in sync.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RawSqlKind {
    /// psql meta-commands (e.g. `\\dt`) — executor skips with a clear message.
    PsqlMetaCommand,
    /// COPY / COPY FROM STDIN — handled by pgwire COPY flow or rejected clearly.
    Copy,
    CreateDatabase,
    DropDatabase,
    AlterDatabase,
    AlterDefaultPrivileges,
    CreateExtension,
    DropExtension,
    CommentOn,
    CreateFunction,
    DropFunction,
    CreateTrigger,
    DropTrigger,
    AlterOwnerTo,
    AlterSequenceOwnedBy,
    RefreshMaterializedView,
    DropMaterializedView,
    Call,
    DropProcedure,
    CreateProcedure,
    CreateTypeEnum,
    DropType,
    /// `RESET <guc>` or `RESET ALL` — handled directly by executor (bypasses
    /// sqlparser which does not support standalone `RESET`).  `RESET ROLE` is
    /// excluded: it is rewritten to `SET ROLE NONE` in the parser layer.
    Reset,
    /// `ANALYZE [table]` — collects table statistics for the query planner.
    /// All syntax validation (VERBOSE, quoted identifiers, trailing junk) is
    /// handled by `parse_analyze_table_name()` in the handler, not here.
    Analyze,
    /// Statements that we accept past Parse so the executor can return a stable
    /// "not supported" error (instead of a syntax error).
    UnsupportedExecutorSkips,
}

/// Find the end of a `/* */` block comment with PostgreSQL-style nesting.
///
/// `s` starts just after the opening `/*`. Returns the byte offset past the
/// closing `*/`, or `None` if the comment is unterminated.
fn find_block_comment_end(s: &str) -> Option<usize> {
    let bytes = s.as_bytes();
    let mut depth: u32 = 1;
    let mut i = 0;
    while i < bytes.len() {
        if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            depth += 1;
            i += 2;
        } else if i + 1 < bytes.len() && bytes[i] == b'*' && bytes[i + 1] == b'/' {
            depth -= 1;
            i += 2;
            if depth == 0 {
                return Some(i);
            }
        } else {
            i += 1;
        }
    }
    None // unterminated
}

/// Skip whitespace and SQL comments (line and block, with nesting).
///
/// Returns `None` for unterminated `/* */`. A line comment that reaches EOF
/// returns `Some("")`.
pub(crate) fn skip_ws_and_comments(s: &str) -> Option<&str> {
    let mut rest = s;
    loop {
        rest = rest.trim_start();
        if rest.starts_with("--") {
            // Line comment: skip to end of line (or end of string).
            match rest.find('\n') {
                Some(pos) => rest = &rest[pos + 1..],
                None => return Some(""),
            }
        } else if rest.starts_with("/*") {
            let after_open = &rest[2..];
            match find_block_comment_end(after_open) {
                Some(end) => rest = &after_open[end..],
                None => return None, // unterminated
            }
        } else {
            return Some(rest);
        }
    }
}

/// Validate the remainder after a GUC name.
///
/// Accepts any mix of whitespace, SQL comments (with nesting), and semicolons.
/// Returns `false` for unexpected tokens or unterminated `/*`.
fn is_valid_tail(tail: &str) -> bool {
    let mut rest = tail;
    loop {
        match skip_ws_and_comments(rest) {
            None => return false, // unterminated block comment
            Some(s) => rest = s,
        }
        if rest.is_empty() {
            return true;
        }
        if rest.starts_with(';') {
            rest = &rest[1..];
        } else {
            return false; // unexpected token
        }
    }
}

/// Extract and validate the GUC name from the text after `RESET`.
///
/// Accepts a single SQL identifier (including dotted names like `tipg.use_optimizer`),
/// optionally followed by `;`, `--` line comment, or `/* */` block comment.
/// PostgreSQL treats comments as whitespace, so comments between `RESET` and the
/// identifier are allowed (e.g. `RESET /*x*/ ALL`).
/// Returns `None` if the input is empty, starts with a non-identifier character,
/// contains unexpected trailing tokens, or has an unterminated block comment.
pub(crate) fn extract_reset_name(after_reset: &str) -> Option<&str> {
    // PostgreSQL treats comments as whitespace — skip them before the identifier.
    let s = skip_ws_and_comments(after_reset)?;
    if s.is_empty() || (!s.as_bytes()[0].is_ascii_alphabetic() && s.as_bytes()[0] != b'_') {
        return None;
    }
    let end = s
        .bytes()
        .position(|b| !b.is_ascii_alphanumeric() && b != b'_' && b != b'.')
        .unwrap_or(s.len());
    let name = &s[..end];
    if is_valid_tail(&s[end..]) {
        Some(name)
    } else {
        None
    }
}

pub(crate) fn classify(sql_upper: &str) -> Option<RawSqlKind> {
    if sql_upper.starts_with('\\') {
        return Some(RawSqlKind::PsqlMetaCommand);
    }
    if sql_upper.starts_with("COPY ") || sql_upper.contains(" FROM STDIN") {
        return Some(RawSqlKind::Copy);
    }

    if sql_upper.starts_with("CREATE DATABASE") {
        return Some(RawSqlKind::CreateDatabase);
    }
    if sql_upper.starts_with("DROP DATABASE") {
        return Some(RawSqlKind::DropDatabase);
    }
    if sql_upper.starts_with("ALTER DATABASE") {
        return Some(RawSqlKind::AlterDatabase);
    }
    if sql_upper.starts_with("ALTER DEFAULT PRIVILEGES") {
        return Some(RawSqlKind::AlterDefaultPrivileges);
    }
    if sql_upper.starts_with("CREATE EXTENSION") {
        return Some(RawSqlKind::CreateExtension);
    }
    if sql_upper.starts_with("DROP EXTENSION") {
        return Some(RawSqlKind::DropExtension);
    }
    if sql_upper.starts_with("COMMENT ON") {
        return Some(RawSqlKind::CommentOn);
    }
    if sql_upper.starts_with("CREATE OR REPLACE FUNCTION")
        || sql_upper.starts_with("CREATE FUNCTION")
    {
        return Some(RawSqlKind::CreateFunction);
    }
    if sql_upper.starts_with("DROP FUNCTION") {
        return Some(RawSqlKind::DropFunction);
    }
    if sql_upper.starts_with("CREATE CONSTRAINT TRIGGER") || sql_upper.starts_with("CREATE TRIGGER")
    {
        return Some(RawSqlKind::CreateTrigger);
    }
    if sql_upper.starts_with("DROP TRIGGER") {
        return Some(RawSqlKind::DropTrigger);
    }
    if (sql_upper.starts_with("ALTER TABLE")
        || sql_upper.starts_with("ALTER SEQUENCE")
        || sql_upper.starts_with("ALTER FUNCTION"))
        && sql_upper.contains(" OWNER TO ")
    {
        return Some(RawSqlKind::AlterOwnerTo);
    }
    if sql_upper.starts_with("ALTER SEQUENCE") && sql_upper.contains("OWNED") {
        return Some(RawSqlKind::AlterSequenceOwnedBy);
    }
    if is_refresh_materialized_view_sql(sql_upper) {
        return Some(RawSqlKind::RefreshMaterializedView);
    }
    if is_drop_materialized_view_sql(sql_upper) {
        return Some(RawSqlKind::DropMaterializedView);
    }
    if sql_upper.starts_with("CALL ") {
        return Some(RawSqlKind::Call);
    }
    if sql_upper.starts_with("DROP PROCEDURE") {
        return Some(RawSqlKind::DropProcedure);
    }
    if sql_upper.starts_with("CREATE PROCEDURE")
        || sql_upper.starts_with("CREATE OR REPLACE PROCEDURE")
    {
        return Some(RawSqlKind::CreateProcedure);
    }
    if is_create_type_as_enum_sql(sql_upper) {
        return Some(RawSqlKind::CreateTypeEnum);
    }
    if sql_upper.starts_with("DROP TYPE") {
        return Some(RawSqlKind::DropType);
    }

    // ANALYZE — keyword boundary only; all syntax validation lives in the handler.
    if sql_upper == "ANALYZE"
        || (sql_upper.len() > 7
            && sql_upper.starts_with("ANALYZE")
            && sql_upper.as_bytes()[7].is_ascii_whitespace())
    {
        return Some(RawSqlKind::Analyze);
    }

    // RESET <guc> / RESET ALL — but NOT RESET ROLE (which is rewritten in the parser).
    // Accept any ASCII whitespace (space, tab, etc.) after "RESET", and validate
    // that only a single identifier token follows (trailing comments/semicolons OK).
    if sql_upper.len() > 5
        && sql_upper[..5].eq_ignore_ascii_case("RESET")
        && sql_upper.as_bytes()[5].is_ascii_whitespace()
    {
        if let Some(name) = extract_reset_name(&sql_upper[5..]) {
            if !name.eq_ignore_ascii_case("ROLE") {
                return Some(RawSqlKind::Reset);
            }
        }
    }

    if is_unsupported_sql_that_executor_skips(sql_upper) {
        return Some(RawSqlKind::UnsupportedExecutorSkips);
    }

    None
}

pub(crate) fn should_accept_sql_without_sqlparser(sql_upper: &str) -> bool {
    classify(sql_upper).is_some()
}

pub(crate) fn skip_reason(sql_upper: &str) -> Option<&'static str> {
    match classify(sql_upper) {
        Some(RawSqlKind::PsqlMetaCommand) => Some("psql meta-command not supported"),
        Some(RawSqlKind::Copy) => Some("COPY not supported"),
        _ => None,
    }
}

pub(crate) fn unsupported_reason(sql_upper: &str) -> Option<&'static str> {
    if !matches!(
        classify(sql_upper),
        Some(RawSqlKind::UnsupportedExecutorSkips)
    ) {
        return None;
    }

    if sql_upper.starts_with("CREATE DOMAIN") {
        return Some("CREATE DOMAIN not supported");
    }
    if sql_upper.starts_with("CREATE AGGREGATE") {
        return Some("CREATE AGGREGATE not supported");
    }
    if sql_upper.starts_with("ALTER TYPE") {
        return Some("ALTER TYPE not supported");
    }
    if sql_upper.starts_with("ALTER DOMAIN") {
        return Some("ALTER DOMAIN not supported");
    }
    if sql_upper.starts_with("ALTER AGGREGATE") {
        return Some("ALTER AGGREGATE not supported");
    }
    if sql_upper.starts_with("ALTER FUNCTION") {
        return Some("ALTER FUNCTION not supported");
    }
    if sql_upper.starts_with("ALTER SEQUENCE") {
        return Some("ALTER SEQUENCE not supported");
    }

    None
}

fn is_refresh_materialized_view_sql(sql_upper: &str) -> bool {
    let mut words = sql_upper.split_whitespace();
    matches!(
        (words.next(), words.next(), words.next()),
        (Some("REFRESH"), Some("MATERIALIZED"), Some("VIEW"))
    )
}

fn is_drop_materialized_view_sql(sql_upper: &str) -> bool {
    let mut words = sql_upper.split_whitespace();
    matches!(
        (words.next(), words.next(), words.next()),
        (Some("DROP"), Some("MATERIALIZED"), Some("VIEW"))
    )
}

fn is_create_type_as_enum_sql(sql_upper: &str) -> bool {
    if !sql_upper.starts_with("CREATE TYPE") {
        return false;
    }
    let mut prev = "";
    for token in sql_upper.split_whitespace() {
        if prev == "AS" && token.starts_with("ENUM") {
            return true;
        }
        prev = token;
    }
    false
}

fn is_unsupported_sql_that_executor_skips(sql_upper: &str) -> bool {
    if sql_upper.starts_with("CREATE DOMAIN") {
        return true;
    }
    if sql_upper.starts_with("CREATE AGGREGATE") {
        return true;
    }
    if sql_upper.starts_with("ALTER TYPE") {
        return true;
    }
    if sql_upper.starts_with("ALTER DOMAIN") {
        return true;
    }
    if sql_upper.starts_with("ALTER AGGREGATE") {
        return true;
    }
    if sql_upper.starts_with("ALTER FUNCTION") {
        return !sql_upper.contains(" OWNER TO ");
    }
    if sql_upper.starts_with("ALTER SEQUENCE") {
        return !sql_upper.contains(" OWNER TO ") && !sql_upper.contains(" OWNED BY ");
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_basic_routes() {
        assert_eq!(
            classify("CREATE DATABASE test"),
            Some(RawSqlKind::CreateDatabase)
        );
        assert_eq!(
            classify("DROP DATABASE test"),
            Some(RawSqlKind::DropDatabase)
        );
        assert_eq!(
            classify("REFRESH MATERIALIZED VIEW mv"),
            Some(RawSqlKind::RefreshMaterializedView)
        );
        assert_eq!(
            classify("DROP MATERIALIZED VIEW mv"),
            Some(RawSqlKind::DropMaterializedView)
        );
        assert_eq!(
            classify("CREATE TYPE t AS ENUM ('a')"),
            Some(RawSqlKind::CreateTypeEnum)
        );
        assert_eq!(classify("SELCT 1"), None);
        // RESET <guc> and RESET ALL are classified as Reset
        assert_eq!(classify("RESET TIMEZONE"), Some(RawSqlKind::Reset));
        assert_eq!(classify("RESET ALL"), Some(RawSqlKind::Reset));
        assert_eq!(
            classify("RESET TIPG.USE_OPTIMIZER;"),
            Some(RawSqlKind::Reset)
        );
        // RESET ROLE is NOT classified as Reset (handled by parser rewrite)
        assert_eq!(classify("RESET ROLE"), None);
    }

    #[test]
    fn extract_reset_name_basic_and_comments() {
        // Basic identifiers
        assert_eq!(extract_reset_name("TIMEZONE"), Some("TIMEZONE"));
        assert_eq!(extract_reset_name("ALL"), Some("ALL"));
        assert_eq!(
            extract_reset_name("TIPG.USE_OPTIMIZER"),
            Some("TIPG.USE_OPTIMIZER")
        );

        // Trailing semicolons
        assert_eq!(
            extract_reset_name("TIPG.USE_OPTIMIZER;"),
            Some("TIPG.USE_OPTIMIZER")
        );

        // Trailing line comments
        assert_eq!(extract_reset_name("TIMEZONE -- note"), Some("TIMEZONE"));
        assert_eq!(extract_reset_name("ALL -- note"), Some("ALL"));

        // Trailing block comments
        assert_eq!(extract_reset_name("ALL /* note */"), Some("ALL"));

        // Leading whitespace (tab, spaces)
        assert_eq!(extract_reset_name("\tTIMEZONE"), Some("TIMEZONE"));
        assert_eq!(extract_reset_name("  TIMEZONE"), Some("TIMEZONE"));

        // Issue 1: junk after comments must be rejected
        assert_eq!(extract_reset_name("ALL /* note */ junk"), None);
        assert_eq!(extract_reset_name("timezone -- note\njunk"), None);
        assert_eq!(extract_reset_name("ALL /* unterminated"), None);

        // Issue 2: comments before identifier (PostgreSQL treats comments as whitespace)
        assert_eq!(extract_reset_name("/*x*/ ALL"), Some("ALL"));
        assert_eq!(extract_reset_name("-- comment\nALL"), Some("ALL"));
        assert_eq!(extract_reset_name("/* unterminated"), None);

        // Nested block comments
        assert_eq!(
            extract_reset_name("/* outer /* inner */ */ ALL"),
            Some("ALL")
        );
        assert_eq!(
            extract_reset_name("ALL /* outer /* inner */ */"),
            Some("ALL")
        );
        assert_eq!(extract_reset_name("ALL /* outer /* inner */"), None); // unterminated nesting

        // Multiple semicolons and mixed trailing
        assert_eq!(extract_reset_name("ALL ; -- comment"), Some("ALL"));
        assert_eq!(extract_reset_name("ALL ; /* note */"), Some("ALL"));

        // Invalid inputs
        assert_eq!(extract_reset_name(""), None);
        assert_eq!(extract_reset_name("123bad"), None);
        assert_eq!(extract_reset_name("   "), None);
    }

    #[test]
    fn classify_reset_with_comments() {
        // Comments after GUC name
        assert_eq!(classify("RESET TIMEZONE -- note"), Some(RawSqlKind::Reset));
        assert_eq!(classify("RESET ALL -- note"), Some(RawSqlKind::Reset));
        assert_eq!(classify("RESET ALL /* note */"), Some(RawSqlKind::Reset));

        // RESET ROLE with trailing comment must NOT be classified as Reset
        assert_eq!(classify("RESET ROLE -- note"), None);

        // Tab whitespace between RESET and GUC name
        assert_eq!(classify("RESET\tTIMEZONE"), Some(RawSqlKind::Reset));

        // Junk after comment must NOT classify as Reset
        assert_eq!(classify("RESET ALL /* note */ junk"), None);

        // Embedded comments between RESET and GUC name
        assert_eq!(classify("RESET /*x*/ ALL"), Some(RawSqlKind::Reset));
    }

    #[test]
    fn skip_and_unsupported_reasons() {
        assert_eq!(
            skip_reason("\\\\dt"),
            Some("psql meta-command not supported")
        );
        assert_eq!(skip_reason("COPY t FROM STDIN"), Some("COPY not supported"));

        assert_eq!(
            unsupported_reason("CREATE DOMAIN foo"),
            Some("CREATE DOMAIN not supported")
        );
        assert_eq!(unsupported_reason("SELECT 1"), None);
    }
}
