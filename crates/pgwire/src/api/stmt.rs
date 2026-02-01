use std::sync::Arc;

use async_trait::async_trait;
use postgres_types::Type;

use crate::error::{ErrorInfo, PgWireError, PgWireResult};
use crate::messages::extendedquery::Parse;

use super::DEFAULT_NAME;

fn is_ident_byte(b: u8) -> bool {
    // NOTE: We intentionally treat any non-ASCII byte as an identifier character.
    // PostgreSQL's lexer allows `\200-\377` inside unquoted identifiers, and we scan raw
    // bytes (UTF-8) here rather than Unicode scalar values. This prevents sequences like
    // `租户$$`/`租户$tag$` from being misread as dollar-quoted strings and hiding `;`.
    b.is_ascii_alphanumeric() || b == b'_' || b == b'$' || b >= 0x80
}

fn is_dollar_quote_tag_byte(b: u8) -> bool {
    // Same idea as `is_ident_byte`, but dollar-quote tags cannot contain `$`.
    b.is_ascii_alphanumeric() || b == b'_' || b >= 0x80
}

fn is_multi_statement_prepared_query(sql: &str) -> bool {
    let bytes = sql.as_bytes();
    let mut i = 0;

    let mut in_single_quote = false;
    let mut single_quote_uses_backslash_escapes = false;
    let mut in_double_quote = false;
    let mut dollar_delim: Option<Vec<u8>> = None;
    let mut in_line_comment = false;
    let mut block_comment_depth: usize = 0;

    let mut seen_stmt_terminator = false;

    while i < bytes.len() {
        let b = bytes[i];

        if in_line_comment {
            if b == b'\n' {
                in_line_comment = false;
            }
            i += 1;
            continue;
        }

        if block_comment_depth > 0 {
            if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
                block_comment_depth += 1;
                i += 2;
                continue;
            }
            if b == b'*' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
                block_comment_depth -= 1;
                i += 2;
                continue;
            }
            i += 1;
            continue;
        }

        if let Some(ref delim) = dollar_delim {
            if i + delim.len() <= bytes.len() && &bytes[i..i + delim.len()] == delim.as_slice() {
                i += delim.len();
                dollar_delim = None;
                continue;
            }
            i += 1;
            continue;
        }

        if in_single_quote {
            if single_quote_uses_backslash_escapes && b == b'\\' {
                i += 1;
                if i < bytes.len() {
                    i += 1;
                }
                continue;
            }
            if b == b'\'' {
                if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                    i += 2;
                    continue;
                }
                in_single_quote = false;
                single_quote_uses_backslash_escapes = false;
            }
            i += 1;
            continue;
        }

        if in_double_quote {
            if b == b'"' {
                if i + 1 < bytes.len() && bytes[i + 1] == b'"' {
                    i += 2;
                    continue;
                }
                in_double_quote = false;
            }
            i += 1;
            continue;
        }

        if seen_stmt_terminator {
            if b.is_ascii_whitespace() || b == b';' {
                i += 1;
                continue;
            }
            if b == b'-' && i + 1 < bytes.len() && bytes[i + 1] == b'-' {
                in_line_comment = true;
                i += 2;
                continue;
            }
            if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
                block_comment_depth = 1;
                i += 2;
                continue;
            }
            return true;
        }

        if b == b'-' && i + 1 < bytes.len() && bytes[i + 1] == b'-' {
            in_line_comment = true;
            i += 2;
            continue;
        }

        if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            block_comment_depth = 1;
            i += 2;
            continue;
        }

        if b == b'\'' {
            in_single_quote = true;
            single_quote_uses_backslash_escapes = i > 0
                && (bytes[i - 1] == b'E' || bytes[i - 1] == b'e')
                && (i < 2 || !(bytes[i - 2].is_ascii_alphanumeric() || bytes[i - 2] == b'_'));
            i += 1;
            continue;
        }

        if b == b'"' {
            in_double_quote = true;
            i += 1;
            continue;
        }

        if b == b'$' {
            // Only treat `$...$` as a dollar-quote delimiter when the `$` is not part of an
            // identifier token. This prevents identifiers like `a$$`/`a$tag$` from hiding `;`.
            let prev_is_ident = i > 0 && is_ident_byte(bytes[i - 1]);
            if !prev_is_ident {
                let mut j = i + 1;
                while j < bytes.len()
                    && bytes[j] != b'$'
                    && is_dollar_quote_tag_byte(bytes[j])
                {
                    j += 1;
                }
                if j < bytes.len() && bytes[j] == b'$' {
                    dollar_delim = Some(bytes[i..=j].to_vec());
                    i = j + 1;
                    continue;
                }
            }
        }

        if b == b';' {
            seen_stmt_terminator = true;
            i += 1;
            continue;
        }

        i += 1;
    }

    false
}

#[non_exhaustive]
#[derive(Debug, Default, new)]
pub struct StoredStatement<S> {
    /// name of the statement
    pub id: String,
    /// parsed query statement
    pub statement: S,
    /// type ids of query parameters, can be empty if frontend asks backend for
    /// type inference
    pub parameter_types: Vec<Type>,
}

impl<S> StoredStatement<S> {
    pub(crate) async fn parse<Q>(parse: &Parse, parser: Q) -> PgWireResult<StoredStatement<S>>
    where
        Q: QueryParser<Statement = S>,
    {
        if is_multi_statement_prepared_query(&parse.query) {
            return Err(PgWireError::UserError(Box::new(ErrorInfo::new(
                "ERROR".to_owned(),
                "42601".to_owned(),
                "cannot insert multiple commands into a prepared statement".to_owned(),
            ))));
        }

        let types = parse
            .type_oids
            .iter()
            .map(|oid| Type::from_oid(*oid).unwrap_or(Type::UNKNOWN))
            .collect::<Vec<Type>>();
        let statement = parser.parse_sql(&parse.query, &types).await?;
        Ok(StoredStatement {
            id: parse
                .name
                .clone()
                .unwrap_or_else(|| DEFAULT_NAME.to_owned()),
            statement,
            parameter_types: types,
        })
    }
}

/// Trait for sql parser. The parser transforms string query into its statement
/// type.
#[async_trait]
pub trait QueryParser {
    type Statement;

    async fn parse_sql(&self, sql: &str, types: &[Type]) -> PgWireResult<Self::Statement>;
}

#[async_trait]
impl<QP> QueryParser for Arc<QP>
where
    QP: QueryParser + Send + Sync,
{
    type Statement = QP::Statement;

    async fn parse_sql(&self, sql: &str, types: &[Type]) -> PgWireResult<Self::Statement> {
        (**self).parse_sql(sql, types).await
    }
}

/// A demo parser implementation. Never use it in serious application.
#[derive(new, Debug, Default)]
pub struct NoopQueryParser;

#[async_trait]
impl QueryParser for NoopQueryParser {
    type Statement = String;

    async fn parse_sql(&self, sql: &str, _types: &[Type]) -> PgWireResult<Self::Statement> {
        Ok(sql.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messages::extendedquery::Parse;

    #[tokio::test]
    async fn rejects_multi_statement_prepared_query() {
        for sql in [
            "SELECT 1; SELECT 2",
            "SELECT E'abc\\''; SELECT 2",
            "SELECT a$$; SELECT 2",
            "SELECT a$$$; SELECT 2",
            "SELECT a$tag$; SELECT 2",
            "SELECT 租户$$; SELECT 2",
            "SELECT 租户$tag$; SELECT 2",
        ] {
            let parse = Parse::new(Some("stmt".to_owned()), sql.to_owned(), vec![]);
            let err = StoredStatement::<String>::parse(&parse, NoopQueryParser::new())
                .await
                .unwrap_err();

            match err {
                PgWireError::UserError(info) => {
                    assert_eq!(
                        info.message,
                        "cannot insert multiple commands into a prepared statement"
                    );
                    assert_eq!(info.code, "42601");
                }
                other => panic!("unexpected error: {other:?}"),
            }
        }
    }

    #[tokio::test]
    async fn allows_trailing_semicolons_and_quoted_semicolons() {
        for sql in [
            "SELECT 1;",
            "SELECT ';' AS semi;",
            "SELECT E';' AS semi;",
            "SELECT E'it\\'s fine; really' AS semi;",
            "SELECT E'\\\\' AS bs;",
            "SELECT $$a;b$$;",
            "SELECT 1; -- trailing comment\n",
        ] {
            let parse = Parse::new(Some("stmt".to_owned()), sql.to_owned(), vec![]);
            StoredStatement::<String>::parse(&parse, NoopQueryParser::new())
                .await
                .unwrap();
        }
    }
}
