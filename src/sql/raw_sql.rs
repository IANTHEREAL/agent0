//! Classification of statements that may bypass `sqlparser` parsing.
//!
//! db9 historically allowed a small set of PostgreSQL statements to reach the
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
    AlterType,
    DropType,
    CreateCollation,
    DropCollation,
    /// `CREATE TEXT SEARCH CONFIGURATION <name> ...` — creates a user-defined
    /// FTS config→tokenizer mapping (zhparser compat; others → 0A000).
    CreateTextSearchConfiguration,
    /// `DROP TEXT SEARCH CONFIGURATION [IF EXISTS] <name>`
    DropTextSearchConfiguration,
    /// `ALTER TEXT SEARCH CONFIGURATION <name> ...` — handles ADD MAPPING etc.
    AlterTextSearchConfiguration,
    /// `RESET <guc>` or `RESET ALL` — handled directly by executor (bypasses
    /// sqlparser which does not support standalone `RESET`).  `RESET ROLE` is
    /// excluded: it is rewritten to `SET ROLE NONE` in the parser layer.
    Reset,
    /// `ALTER SYSTEM SET <guc> = <value>` — server-level config change.
    AlterSystemSet,
    /// `DO $$ ... $$` — anonymous PL/pgSQL block.
    Do,
    /// `ANALYZE [table]` — collects table statistics for the query planner.
    /// All syntax validation (VERBOSE, quoted identifiers, trailing junk) is
    /// handled by `parse_analyze_table_name()` in the handler, not here.
    Analyze,
    /// `ALTER INDEX IF EXISTS <name> RENAME TO <name>` — sqlparser 0.40 can't
    /// parse `IF EXISTS` after `ALTER INDEX`. Intercepted here and dispatched
    /// to a raw-SQL handler that extracts index names manually.
    AlterIndexIfExists,
    /// `CREATE POLICY <name> ON <table> ...` — RLS policy creation.
    CreatePolicy,
    /// `ALTER POLICY <name> ON <table> ...` — RLS policy modification.
    AlterPolicy,
    /// `DROP POLICY [IF EXISTS] <name> ON <table>` — RLS policy removal.
    DropPolicy,
    /// `ALTER TABLE <table> {ENABLE|DISABLE|FORCE|NO FORCE} ROW LEVEL SECURITY`
    AlterTableRls,
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

/// Parsed result from `extract_reset_name` / `parse_reset_var_name`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ResetName {
    /// Normalized name: lowercase, dotted segments joined (e.g. `session.authorization`).
    pub name: String,
    /// Original token text preserving quoted case (e.g. `"IS_SUPERUSER"`, `"AlL"`).
    /// For unquoted identifiers this is the same as `name`.
    pub original: String,
    /// Whether the first (or only) segment was a quoted identifier.
    pub first_quoted: bool,
}

/// Parse a quoted identifier starting at `"`.  Returns `(original_content, lowercase_content, rest)`
/// where `rest` is the slice after the closing quote.  Handles `""` escape.
/// Rejects zero-length identifiers (`""`) to match PostgreSQL.
fn parse_quoted_identifier(s: &str) -> Option<(String, String, &str)> {
    debug_assert!(s.starts_with('"'));
    let inner = &s[1..];
    let mut chars = inner.char_indices();
    let mut original = String::new();
    while let Some((i, ch)) = chars.next() {
        if ch == '"' {
            let rest_after_quote = &inner[i + 1..];
            if rest_after_quote.starts_with('"') {
                // Escaped double-quote (`""`)
                original.push('"');
                chars.next(); // skip the second quote
            } else {
                // End of identifier — reject zero-length
                if original.is_empty() {
                    return None;
                }
                let lowercase = original.to_lowercase();
                return Some((original, lowercase, rest_after_quote));
            }
        } else {
            original.push(ch);
        }
    }
    None // unterminated
}

/// Check if an uppercase word is a PostgreSQL 17 keyword that cannot be used
/// as an unquoted `ColId` (reserved_keyword + type_func_name_keyword).
fn is_pg_non_colid_keyword(upper: &str) -> bool {
    matches!(
        upper,
        // ── reserved_keyword (PG 17 kwlist.h) ──
        "ALL"
            | "ANALYSE"
            | "ANALYZE"
            | "AND"
            | "ANY"
            | "ARRAY"
            | "AS"
            | "ASC"
            | "ASYMMETRIC"
            | "BOTH"
            | "CASE"
            | "CAST"
            | "CHECK"
            | "COLLATE"
            | "COLUMN"
            | "CONSTRAINT"
            | "CREATE"
            | "CURRENT_CATALOG"
            | "CURRENT_DATE"
            | "CURRENT_ROLE"
            | "CURRENT_TIME"
            | "CURRENT_TIMESTAMP"
            | "CURRENT_USER"
            | "DEFAULT"
            | "DEFERRABLE"
            | "DESC"
            | "DISTINCT"
            | "DO"
            | "ELSE"
            | "END"
            | "EXCEPT"
            | "FALSE"
            | "FETCH"
            | "FOR"
            | "FOREIGN"
            | "FROM"
            | "GRANT"
            | "GROUP"
            | "HAVING"
            | "IN"
            | "INITIALLY"
            | "INTERSECT"
            | "INTO"
            | "LATERAL"
            | "LEADING"
            | "LIMIT"
            | "LOCALTIME"
            | "LOCALTIMESTAMP"
            | "NOT"
            | "NULL"
            | "OFFSET"
            | "ON"
            | "ONLY"
            | "OR"
            | "ORDER"
            | "PLACING"
            | "PRIMARY"
            | "REFERENCES"
            | "RETURNING"
            | "SELECT"
            | "SESSION_USER"
            | "SOME"
            | "SYMMETRIC"
            | "SYSTEM_USER"
            | "TABLE"
            | "THEN"
            | "TO"
            | "TRAILING"
            | "TRUE"
            | "UNION"
            | "UNIQUE"
            | "USER"
            | "USING"
            | "VARIADIC"
            | "WHEN"
            | "WHERE"
            | "WINDOW"
            | "WITH"
            // ── type_func_name_keyword (PG 17 kwlist.h) ──
            | "AUTHORIZATION"
            | "BINARY"
            | "COLLATION"
            | "CONCURRENTLY"
            | "CROSS"
            | "CURRENT_SCHEMA"
            | "FREEZE"
            | "FULL"
            | "ILIKE"
            | "INNER"
            | "IS"
            | "ISNULL"
            | "JOIN"
            | "LEFT"
            | "LIKE"
            | "NATURAL"
            | "NOTNULL"
            | "OUTER"
            | "OVERLAPS"
            | "RIGHT"
            | "SIMILAR"
            | "TABLESAMPLE"
            | "VERBOSE"
    )
}

/// Parse a single `ColId` segment: quoted identifier or unquoted identifier.
///
/// When `check_keywords` is true, rejects unquoted words that are PG non-ColId
/// keywords (e.g. `AUTHORIZATION`, `DEFAULT`, `ALL`).
///
/// Returns `(original_text, normalized_lowercase_text, was_quoted, remaining_input)`.
fn parse_colid_segment(s: &str, check_keywords: bool) -> Option<(String, String, bool, &str)> {
    if s.starts_with('"') {
        let (original, lowercase, rest) = parse_quoted_identifier(s)?;
        Some((original, lowercase, true, rest))
    } else if s.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_') {
        let end = s
            .bytes()
            .position(|b| !b.is_ascii_alphanumeric() && b != b'_')
            .unwrap_or(s.len());
        let word = &s[..end];
        if check_keywords && is_pg_non_colid_keyword(&word.to_ascii_uppercase()) {
            return None;
        }
        let lower = word.to_ascii_lowercase();
        Some((lower.clone(), lower, false, &s[end..]))
    } else {
        None
    }
}

/// Core parser for a PostgreSQL `var_name`: `ColId { '.' ColId }*`.
///
/// `check_keywords`: when true, unquoted non-ColId keywords are rejected.
fn parse_reset_target(after_reset: &str, check_keywords: bool) -> Option<ResetName> {
    let s = skip_ws_and_comments(after_reset)?;
    if s.is_empty() {
        return None;
    }
    let (first_orig, first_lower, first_quoted, mut rest) = parse_colid_segment(s, check_keywords)?;
    let mut name = first_lower;
    let mut original = if first_quoted {
        format!("\"{}\"", first_orig)
    } else {
        first_orig
    };
    loop {
        match skip_ws_and_comments(rest) {
            None => return None,
            Some(r) => rest = r,
        }
        if rest.starts_with('.') {
            let after_dot = &rest[1..];
            let s2 = skip_ws_and_comments(after_dot)?;
            let (seg_orig, seg_lower, seg_quoted, r) = parse_colid_segment(s2, check_keywords)?;
            name.push('.');
            name.push_str(&seg_lower);
            original.push('.');
            if seg_quoted {
                original.push('"');
                original.push_str(&seg_orig);
                original.push('"');
            } else {
                original.push_str(&seg_orig);
            }
            rest = r;
        } else {
            break;
        }
    }
    if is_valid_tail(rest) {
        Some(ResetName {
            name,
            original,
            first_quoted,
        })
    } else {
        None
    }
}

/// Permissive parser for `classify`: accepts any identifier token including
/// reserved keywords (so that `RESET ALL` is recognized as the Reset kind).
pub(crate) fn extract_reset_name(after_reset: &str) -> Option<ResetName> {
    parse_reset_target(after_reset, false)
}

/// Strict parser for `execute_reset`: rejects unquoted non-ColId keywords
/// (e.g. `RESET DEFAULT` → syntax error, matching PostgreSQL).
pub(crate) fn parse_reset_var_name(after_reset: &str) -> Option<ResetName> {
    parse_reset_target(after_reset, true)
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
    if sql_upper.starts_with("CREATE POLICY") {
        return Some(RawSqlKind::CreatePolicy);
    }
    if sql_upper.starts_with("ALTER POLICY") {
        return Some(RawSqlKind::AlterPolicy);
    }
    if sql_upper.starts_with("DROP POLICY") {
        return Some(RawSqlKind::DropPolicy);
    }
    // ALTER TABLE ... {ENABLE|DISABLE|FORCE|NO FORCE} ROW LEVEL SECURITY
    // Must be checked before generic ALTER TABLE handlers.
    if sql_upper.starts_with("ALTER TABLE") && sql_upper.contains("ROW LEVEL SECURITY") {
        return Some(RawSqlKind::AlterTableRls);
    }
    if sql_upper.starts_with("ALTER SYSTEM SET ") {
        return Some(RawSqlKind::AlterSystemSet);
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
    if sql_upper.starts_with("ALTER TYPE") {
        return Some(RawSqlKind::AlterType);
    }
    if sql_upper.starts_with("DROP TYPE") {
        return Some(RawSqlKind::DropType);
    }
    if sql_upper.starts_with("ALTER INDEX IF EXISTS") {
        return Some(RawSqlKind::AlterIndexIfExists);
    }
    if sql_upper.starts_with("CREATE COLLATION") {
        return Some(RawSqlKind::CreateCollation);
    }
    if sql_upper.starts_with("DROP COLLATION") {
        return Some(RawSqlKind::DropCollation);
    }

    // Text search configuration DDL — multi-word prefix matching.
    if is_create_text_search_configuration(sql_upper) {
        return Some(RawSqlKind::CreateTextSearchConfiguration);
    }
    if is_drop_text_search_configuration(sql_upper) {
        return Some(RawSqlKind::DropTextSearchConfiguration);
    }
    if is_alter_text_search_configuration(sql_upper) {
        return Some(RawSqlKind::AlterTextSearchConfiguration);
    }

    // DO block: keyword boundary ensures we don't match DOCUMENT, DOUBLE, etc.
    if sql_upper.len() > 2
        && sql_upper.starts_with("DO")
        && !sql_upper.as_bytes()[2].is_ascii_alphanumeric()
        && sql_upper.as_bytes()[2] != b'_'
    {
        return Some(RawSqlKind::Do);
    }

    // ANALYZE — keyword boundary only; all syntax validation lives in the handler.
    if sql_upper == "ANALYZE"
        || (sql_upper.len() > 7
            && sql_upper.starts_with("ANALYZE")
            && sql_upper.as_bytes()[7].is_ascii_whitespace())
    {
        return Some(RawSqlKind::Analyze);
    }

    // RESET <guc> / RESET ALL / RESET "quoted" — but NOT unquoted RESET ROLE
    // (which is rewritten in the parser). Double-quoted ROLE is treated as a
    // regular GUC name, not the keyword.
    if sql_upper.len() > 5
        && sql_upper[..5].eq_ignore_ascii_case("RESET")
        && sql_upper.as_bytes()[5].is_ascii_whitespace()
    {
        if let Some(rn) = extract_reset_name(&sql_upper[5..]) {
            // Unquoted ROLE is rewritten to SET ROLE NONE in the parser layer;
            // quoted "ROLE" is a regular parameter name and should be handled here.
            if rn.name != "role" || rn.first_quoted {
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

fn is_create_text_search_configuration(sql_upper: &str) -> bool {
    let mut words = sql_upper.split_whitespace();
    matches!(
        (words.next(), words.next(), words.next(), words.next()),
        (
            Some("CREATE"),
            Some("TEXT"),
            Some("SEARCH"),
            Some("CONFIGURATION")
        )
    )
}

fn is_drop_text_search_configuration(sql_upper: &str) -> bool {
    let mut words = sql_upper.split_whitespace();
    matches!(
        (words.next(), words.next(), words.next(), words.next()),
        (
            Some("DROP"),
            Some("TEXT"),
            Some("SEARCH"),
            Some("CONFIGURATION")
        )
    )
}

fn is_alter_text_search_configuration(sql_upper: &str) -> bool {
    let mut words = sql_upper.split_whitespace();
    matches!(
        (words.next(), words.next(), words.next(), words.next()),
        (
            Some("ALTER"),
            Some("TEXT"),
            Some("SEARCH"),
            Some("CONFIGURATION")
        )
    )
}

fn is_unsupported_sql_that_executor_skips(sql_upper: &str) -> bool {
    if sql_upper.starts_with("CREATE DOMAIN") {
        return true;
    }
    if sql_upper.starts_with("CREATE AGGREGATE") {
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
        assert_eq!(
            classify("ALTER TYPE role ADD VALUE 'MODERATOR'"),
            Some(RawSqlKind::AlterType)
        );
        assert_eq!(
            classify("ALTER TYPE role RENAME TO new_role"),
            Some(RawSqlKind::AlterType)
        );
        // ALTER INDEX IF EXISTS classification (#885)
        assert_eq!(
            classify("ALTER INDEX IF EXISTS \"IDX_OLD\" RENAME TO \"IDX_NEW\""),
            Some(RawSqlKind::AlterIndexIfExists)
        );
        assert_eq!(classify("SELCT 1"), None);
        // ALTER SYSTEM SET classification
        assert_eq!(
            classify("ALTER SYSTEM SET STATEMENT_TIMEOUT = '5S'"),
            Some(RawSqlKind::AlterSystemSet)
        );
        assert_eq!(
            classify("ALTER SYSTEM SET IDLE_IN_TRANSACTION_SESSION_TIMEOUT TO '10S'"),
            Some(RawSqlKind::AlterSystemSet)
        );
        // ALTER SYSTEM without SET does NOT match AlterSystemSet
        assert!(!matches!(
            classify("ALTER SYSTEM RESET ALL"),
            Some(RawSqlKind::AlterSystemSet)
        ));
        // RESET <guc> and RESET ALL are classified as Reset
        assert_eq!(classify("RESET TIMEZONE"), Some(RawSqlKind::Reset));
        assert_eq!(classify("RESET ALL"), Some(RawSqlKind::Reset));
        assert_eq!(
            classify("RESET DB9.USE_OPTIMIZER;"),
            Some(RawSqlKind::Reset)
        );
        // RESET ROLE is NOT classified as Reset (handled by parser rewrite)
        assert_eq!(classify("RESET ROLE"), None);
        // Double-quoted identifiers are classified as Reset (#1662)
        assert_eq!(classify("RESET \"ALL\""), Some(RawSqlKind::Reset));
        assert_eq!(classify("RESET \"timezone\""), Some(RawSqlKind::Reset));
        // Quoted ROLE IS classified as Reset (quoting prevents keyword interpretation)
        assert_eq!(classify("RESET \"ROLE\""), Some(RawSqlKind::Reset));
    }

    /// Helper: assert that `extract_reset_name` returns a name with
    /// `first_quoted == false` and the expected lowercase name.
    fn assert_unquoted(input: &str, expected_name: &str) {
        let rn = extract_reset_name(input).unwrap_or_else(|| {
            panic!("expected Some for {:?}, got None", input);
        });
        assert_eq!(rn.name, expected_name, "name mismatch for {:?}", input);
        assert!(!rn.first_quoted, "expected unquoted for {:?}", input);
    }

    #[test]
    fn extract_reset_name_basic_and_comments() {
        // Basic identifiers (permissive: accepts keywords like ALL)
        assert_unquoted("TIMEZONE", "timezone");
        assert_unquoted("ALL", "all");
        assert_unquoted("DB9.USE_OPTIMIZER", "db9.use_optimizer");

        // Trailing semicolons
        assert_unquoted("DB9.USE_OPTIMIZER;", "db9.use_optimizer");

        // Trailing line comments
        assert_unquoted("TIMEZONE -- note", "timezone");
        assert_unquoted("ALL -- note", "all");

        // Trailing block comments
        assert_unquoted("ALL /* note */", "all");

        // Leading whitespace (tab, spaces)
        assert_unquoted("\tTIMEZONE", "timezone");
        assert_unquoted("  TIMEZONE", "timezone");

        // Issue 1: junk after comments must be rejected
        assert_eq!(extract_reset_name("ALL /* note */ junk"), None);
        assert_eq!(extract_reset_name("timezone -- note\njunk"), None);
        assert_eq!(extract_reset_name("ALL /* unterminated"), None);

        // Issue 2: comments before identifier
        assert_unquoted("/*x*/ ALL", "all");
        assert_unquoted("-- comment\nALL", "all");
        assert_eq!(extract_reset_name("/* unterminated"), None);

        // Nested block comments
        assert_unquoted("/* outer /* inner */ */ ALL", "all");
        assert_unquoted("ALL /* outer /* inner */ */", "all");
        assert_eq!(extract_reset_name("ALL /* outer /* inner */"), None);

        // Multiple semicolons and mixed trailing
        assert_unquoted("ALL ; -- comment", "all");
        assert_unquoted("ALL ; /* note */", "all");

        // Invalid inputs
        assert_eq!(extract_reset_name(""), None);
        assert_eq!(extract_reset_name("123bad"), None);
        assert_eq!(extract_reset_name("   "), None);

        // Double-quoted identifiers (#1662) — covered in detail by
        // `extract_reset_name_quoted_identifiers`; quick smoke here.
        let rn = extract_reset_name("\"ALL\"").unwrap();
        assert_eq!(rn.name, "all");
        assert!(rn.first_quoted);

        // Invalid quoted identifiers
        assert_eq!(extract_reset_name("\"\""), None); // empty
        assert_eq!(extract_reset_name("\"unterminated"), None);
        assert_eq!(extract_reset_name("\"ALL\" junk"), None);
    }

    #[test]
    fn extract_reset_name_quoted_identifiers() {
        // Simple quoted identifier — preserves original case
        let rn = extract_reset_name("\"is_superuser\"").unwrap();
        assert_eq!(rn.name, "is_superuser");
        assert_eq!(rn.original, "\"is_superuser\"");
        assert!(rn.first_quoted);

        // Quoted ALL — original preserves uppercase
        let rn = extract_reset_name("\"ALL\"").unwrap();
        assert_eq!(rn.name, "all");
        assert_eq!(rn.original, "\"ALL\"");
        assert!(rn.first_quoted);

        // Mixed case quoted — original preserves exact spelling
        let rn = extract_reset_name("\"AlL\"").unwrap();
        assert_eq!(rn.name, "all");
        assert_eq!(rn.original, "\"AlL\"");
        assert!(rn.first_quoted);

        // Quoted IS_SUPERUSER — original preserves uppercase
        let rn = extract_reset_name("\"IS_SUPERUSER\"").unwrap();
        assert_eq!(rn.name, "is_superuser");
        assert_eq!(rn.original, "\"IS_SUPERUSER\"");
        assert!(rn.first_quoted);

        // Fully qualified dotted with both segments quoted
        let rn = extract_reset_name("\"session\".\"authorization\"").unwrap();
        assert_eq!(rn.name, "session.authorization");
        assert_eq!(rn.original, "\"session\".\"authorization\"");
        assert!(rn.first_quoted);

        // Mixed: quoted first, unquoted second
        let rn = extract_reset_name("\"session\".foo").unwrap();
        assert_eq!(rn.name, "session.foo");
        assert_eq!(rn.original, "\"session\".foo");
        assert!(rn.first_quoted);

        // Mixed: unquoted first, quoted second
        let rn = extract_reset_name("foo.\"bar\"").unwrap();
        assert_eq!(rn.name, "foo.bar");
        assert_eq!(rn.original, "foo.\"bar\"");
        assert!(!rn.first_quoted);

        // Quoted with escaped double quotes
        let rn = extract_reset_name("\"a\"\"b\"").unwrap();
        assert_eq!(rn.name, "a\"b");
        assert!(rn.first_quoted);

        // Quoted with trailing semicolons/comments
        let rn = extract_reset_name("\"is_superuser\"; -- note").unwrap();
        assert_eq!(rn.name, "is_superuser");
        assert!(rn.first_quoted);

        // Unterminated quoted identifier
        assert_eq!(extract_reset_name("\"unterminated"), None);
    }

    #[test]
    fn reject_zero_length_quoted_identifiers() {
        // PostgreSQL rejects zero-length delimited identifiers
        assert_eq!(extract_reset_name("\"\""), None);
        assert_eq!(extract_reset_name("\"\".foo"), None);
        assert_eq!(extract_reset_name("foo.\"\""), None);
        assert_eq!(parse_reset_var_name("\"\""), None);
        assert_eq!(parse_reset_var_name("\"\".\"x\""), None);
    }

    #[test]
    fn utf8_quoted_identifiers_preserved() {
        // UTF-8 content must not be corrupted
        let rn = extract_reset_name("\"café\"").unwrap();
        assert_eq!(rn.name, "café");
        assert_eq!(rn.original, "\"café\"");
        assert!(rn.first_quoted);

        let rn = extract_reset_name("\"日本語\"").unwrap();
        assert_eq!(rn.name, "日本語");
        assert!(rn.first_quoted);
    }

    #[test]
    fn parse_reset_var_name_strict_keyword_rejection() {
        // Strict mode rejects unquoted non-ColId keywords
        assert!(parse_reset_var_name("DEFAULT").is_none());
        assert!(parse_reset_var_name("ALL").is_none());
        assert!(parse_reset_var_name("AUTHORIZATION").is_none());
        assert!(parse_reset_var_name("SELECT").is_none());

        // But quoted keywords are fine
        assert!(parse_reset_var_name("\"ALL\"").is_some());
        assert!(parse_reset_var_name("\"DEFAULT\"").is_some());
        assert!(parse_reset_var_name("\"AUTHORIZATION\"").is_some());

        // Mixed dotted: unquoted non-ColId keyword in second segment
        assert!(parse_reset_var_name("\"session\".authorization").is_none());

        // Mixed dotted: quoted keyword in second segment is fine
        let rn = parse_reset_var_name("\"session\".\"authorization\"").unwrap();
        assert_eq!(rn.name, "session.authorization");

        // Regular identifiers pass strict mode
        let rn = parse_reset_var_name("timezone").unwrap();
        assert_eq!(rn.name, "timezone");
        assert!(!rn.first_quoted);

        let rn = parse_reset_var_name("db9.use_optimizer").unwrap();
        assert_eq!(rn.name, "db9.use_optimizer");
    }

    #[test]
    fn classify_reset_quoted_identifiers() {
        // Quoted identifiers are now classified as Reset
        assert_eq!(classify("RESET \"IS_SUPERUSER\""), Some(RawSqlKind::Reset));
        assert_eq!(classify("RESET \"ALL\""), Some(RawSqlKind::Reset));

        // Quoted ROLE is classified as Reset (it's a parameter name, not the keyword)
        assert_eq!(classify("RESET \"ROLE\""), Some(RawSqlKind::Reset));

        // Unquoted ROLE is still excluded
        assert_eq!(classify("RESET ROLE"), None);

        // Dotted quoted names
        assert_eq!(
            classify("RESET \"SESSION\".\"AUTHORIZATION\""),
            Some(RawSqlKind::Reset)
        );
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
    fn classify_do_block() {
        assert_eq!(classify("DO $$ BEGIN END $$"), Some(RawSqlKind::Do));
        assert_eq!(classify("DO$$ BEGIN END $$"), Some(RawSqlKind::Do));
        assert_eq!(classify("DO\n$$ BEGIN END $$"), Some(RawSqlKind::Do));
        assert_eq!(
            classify("DO LANGUAGE PLPGSQL $$ BEGIN END $$"),
            Some(RawSqlKind::Do)
        );
        // Must NOT match words that start with DO
        assert_eq!(classify("DOUBLE PRECISION"), None);
        assert_eq!(classify("DOCUMENT"), None);
    }

    #[test]
    fn alter_system_set_accepted_as_raw_utility() {
        assert!(should_accept_sql_without_sqlparser(
            "ALTER SYSTEM SET STATEMENT_TIMEOUT = '5S'"
        ));
        assert!(should_accept_sql_without_sqlparser(
            "ALTER SYSTEM SET IDLE_IN_TRANSACTION_SESSION_TIMEOUT TO '10S'"
        ));
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
