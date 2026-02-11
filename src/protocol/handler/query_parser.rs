use super::errors::syntax_error_pgwire_error;
use async_trait::async_trait;
use pgwire::api::Type;
use pgwire::error::PgWireResult;

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

fn should_accept_sql_without_sqlparser(sql_upper: &str) -> bool {
    // Allow statements that sqlparser cannot parse but the executor handles via raw-SQL
    // interception or returns a clear unsupported error for.
    if sql_upper.starts_with('\\') {
        return true;
    }
    if sql_upper.starts_with("COPY ") || sql_upper.contains(" FROM STDIN") {
        return true;
    }

    if sql_upper.starts_with("CREATE DATABASE")
        || sql_upper.starts_with("DROP DATABASE")
        || sql_upper.starts_with("ALTER DATABASE")
        || sql_upper.starts_with("ALTER DEFAULT PRIVILEGES")
        || sql_upper.starts_with("CREATE EXTENSION")
        || sql_upper.starts_with("DROP EXTENSION")
        || sql_upper.starts_with("COMMENT ON")
        || sql_upper.starts_with("CREATE OR REPLACE FUNCTION")
        || sql_upper.starts_with("CREATE FUNCTION")
        || sql_upper.starts_with("DROP FUNCTION")
        || sql_upper.starts_with("CREATE CONSTRAINT TRIGGER")
        || sql_upper.starts_with("CREATE TRIGGER")
        || sql_upper.starts_with("DROP TRIGGER")
        || ((sql_upper.starts_with("ALTER TABLE")
            || sql_upper.starts_with("ALTER SEQUENCE")
            || sql_upper.starts_with("ALTER FUNCTION"))
            && sql_upper.contains(" OWNER TO "))
        || (sql_upper.starts_with("ALTER SEQUENCE") && sql_upper.contains("OWNED"))
        || is_refresh_materialized_view_sql(sql_upper)
        || is_drop_materialized_view_sql(sql_upper)
        || sql_upper.starts_with("CALL ")
        || sql_upper.starts_with("DROP PROCEDURE")
        || sql_upper.starts_with("CREATE PROCEDURE")
        || sql_upper.starts_with("CREATE OR REPLACE PROCEDURE")
        || is_create_type_as_enum_sql(sql_upper)
        || sql_upper.starts_with("DROP TYPE")
        || is_unsupported_sql_that_executor_skips(sql_upper)
    {
        return true;
    }

    false
}

#[derive(Debug, Default)]
pub struct TipgQueryParser;

impl TipgQueryParser {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl pgwire::api::stmt::QueryParser for TipgQueryParser {
    type Statement = String;

    async fn parse_sql(&self, sql: &str, _types: &[Type]) -> PgWireResult<Self::Statement> {
        // Match libpq behavior for empty queries (handled later by executor/protocol).
        if sql.trim().is_empty() {
            return Ok(sql.to_owned());
        }

        let parse_err = match crate::sql::parse_sql(sql) {
            Ok(_) => return Ok(sql.to_owned()),
            Err(e) => e,
        };

        let Some(sql_no_comments) = strip_leading_whitespace_and_comments(sql) else {
            return Err(syntax_error_pgwire_error(parse_err.to_string()));
        };

        let sql_upper = sql_no_comments.trim_start().to_ascii_uppercase();
        if should_accept_sql_without_sqlparser(&sql_upper) {
            return Ok(sql.to_owned());
        }

        Err(syntax_error_pgwire_error(parse_err.to_string()))
    }
}

pub(super) fn strip_leading_whitespace_and_comments(query: &str) -> Option<&str> {
    let bytes = query.as_bytes();
    let mut i = 0usize;

    while i < bytes.len() {
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }

        if i + 1 < bytes.len() && bytes[i] == b'-' && bytes[i + 1] == b'-' {
            i += 2;
            while i < bytes.len() {
                let is_newline = bytes[i] == b'\n';
                i += 1;
                if is_newline {
                    break;
                }
            }
            continue;
        }

        // Block comment (supports nesting): /* ... */
        if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'*' {
            i += 2;
            let mut depth = 1usize;
            while i < bytes.len() && depth > 0 {
                if i + 1 < bytes.len() && bytes[i] == b'/' && bytes[i + 1] == b'*' {
                    depth += 1;
                    i += 2;
                    continue;
                }
                if i + 1 < bytes.len() && bytes[i] == b'*' && bytes[i + 1] == b'/' {
                    depth -= 1;
                    i += 2;
                    continue;
                }
                i += 1;
            }
            if depth > 0 {
                return None;
            }
            continue;
        }

        break;
    }

    Some(&query[i..])
}
