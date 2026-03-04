use super::errors::syntax_error_pgwire_error;
use super::prepared::{PreparedExec, PreparedStatement};
use async_trait::async_trait;
use pgwire::api::Type;
use pgwire::error::PgWireResult;
use sqlparser::ast::Statement;
use std::sync::Mutex;

/// Query parser for the extended-query protocol.
///
/// `parse_sql` is the **single parse authority** for every extended Parse
/// message.  After `StoredStatement::parse` returns, the caller can retrieve
/// the cached AST via [`take_parsed_statements`] to avoid redundant
/// re-parsing in classification and analysis.
#[derive(Debug, Default)]
pub struct Db9QueryParser {
    /// Cached parse result from the most recent `parse_sql` call.
    /// `Some(stmts)` when the SQL parsed successfully; `None` for empty SQL
    /// or fallback-accepted unparseable SQL.
    last_parsed: Mutex<Option<Vec<Statement>>>,
}

impl Db9QueryParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Take the cached parsed statements from the most recent `parse_sql` call.
    ///
    /// Returns `Some(stmts)` when the SQL was successfully parsed by
    /// `sqlparser`, or `None` when the SQL was empty or accepted only via the
    /// `should_accept_sql_without_sqlparser` fallback.
    ///
    /// This is a destructive read — calling it twice returns `None` the second
    /// time.
    pub(super) fn take_parsed_statements(&self) -> Option<Vec<Statement>> {
        self.last_parsed.lock().unwrap().take()
    }
}

#[async_trait]
impl pgwire::api::stmt::QueryParser for Db9QueryParser {
    type Statement = PreparedStatement;

    async fn parse_sql(&self, sql: &str, _types: &[Type]) -> PgWireResult<Self::Statement> {
        // Match libpq behavior for empty queries (handled later by executor/protocol).
        if sql.trim().is_empty() {
            *self.last_parsed.lock().unwrap() = None;
            return Ok(PreparedStatement {
                sql: sql.to_owned(),
                exec: PreparedExec::RawSqlUtility,
                output_schema: vec![],
                param_data_types: vec![],
                table_versions: vec![],
            });
        }

        let parse_err = match crate::sql::parse_sql(sql) {
            Ok(stmts) => {
                *self.last_parsed.lock().unwrap() = Some(stmts);
                return Ok(PreparedStatement {
                    sql: sql.to_owned(),
                    exec: PreparedExec::RawSqlUtility,
                    output_schema: vec![],
                    param_data_types: vec![],
                    table_versions: vec![],
                });
            }
            Err(e) => e,
        };

        // Parse failed — clear cache before attempting fallback.
        *self.last_parsed.lock().unwrap() = None;

        let Some(sql_no_comments) = strip_leading_whitespace_and_comments(sql) else {
            return Err(syntax_error_pgwire_error(parse_err.to_string()));
        };

        let sql_upper = sql_no_comments.trim_start().to_ascii_uppercase();
        if crate::sql::raw_sql::should_accept_sql_without_sqlparser(&sql_upper) {
            return Ok(PreparedStatement {
                sql: sql.to_owned(),
                exec: PreparedExec::RawSqlUtility,
                output_schema: vec![],
                param_data_types: vec![],
                table_versions: vec![],
            });
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
