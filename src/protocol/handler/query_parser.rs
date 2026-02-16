use super::errors::syntax_error_pgwire_error;
use async_trait::async_trait;
use pgwire::api::Type;
use pgwire::error::PgWireResult;

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
        if crate::sql::raw_sql::should_accept_sql_without_sqlparser(&sql_upper) {
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
