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
    /// Statements that we accept past Parse so the executor can return a stable
    /// "not supported" error (instead of a syntax error).
    UnsupportedExecutorSkips,
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
