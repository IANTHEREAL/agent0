//! CREATE/DROP/ALTER TEXT SEARCH CONFIGURATION executor handlers.
//!
//! Only zhparser-related configurations are accepted (mapped to the built-in
//! jieba tokenizer).  All other parser names return `0A000 feature_not_supported`.

use anyhow::{anyhow, Result};
use tracing::info;

use crate::sql::error::SqlError;

use super::super::{ExecuteResult, Session};
use super::core::starts_with_ignore_ascii_case;
use super::core::Executor;
use super::triggers::strip_leading_sql_comments;

// ---------------------------------------------------------------------------
// SQL parsing helpers
// ---------------------------------------------------------------------------

/// Consume a SQL keyword (case-insensitive) from the start of `input`.
/// Returns the remainder (trimmed) or `None` if the keyword doesn't match.
fn consume_kw<'a>(input: &'a str, keyword: &str) -> Option<&'a str> {
    let input = input.trim_start();
    if input.len() < keyword.len() {
        return None;
    }
    let (head, rest) = input.split_at(keyword.len());
    if !head.eq_ignore_ascii_case(keyword) {
        return None;
    }
    // Ensure word boundary (next char is whitespace, punctuation, or end).
    if let Some(c) = rest.chars().next() {
        if c.is_ascii_alphanumeric() || c == '_' {
            return None;
        }
    }
    Some(rest.trim_start())
}

/// Parse a SQL identifier: plain `name`, double-quoted `"Name"`, or schema-qualified.
/// Returns `(identifier_lowercase, remainder)`.
fn parse_identifier(input: &str) -> Option<(String, &str)> {
    let input = input.trim_start();
    if input.is_empty() {
        return None;
    }

    if let Some(stripped) = input.strip_prefix('"') {
        // Quoted identifier: consume until closing quote (handle "" escaping).
        let mut chars = stripped.chars();
        let mut name = String::new();
        loop {
            match chars.next() {
                Some('"') => {
                    if chars.as_str().starts_with('"') {
                        name.push('"');
                        chars.next(); // skip escaped quote
                    } else {
                        // End of quoted identifier.
                        let rest = chars.as_str();
                        return Some((name.to_lowercase(), rest));
                    }
                }
                Some(c) => name.push(c),
                None => return None, // Unterminated quote.
            }
        }
    } else {
        // Unquoted identifier: word chars only.
        let end = input
            .find(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '.')
            .unwrap_or(input.len());
        if end == 0 {
            return None;
        }
        let name = &input[..end];
        Some((name.to_lowercase(), &input[end..]))
    }
}

/// Extract the parser name from the option list of CREATE TEXT SEARCH CONFIGURATION.
///
/// Recognized patterns:
/// - `( PARSER = zhparser )`
/// - `( PARSER = pg_catalog.zhparser )`
/// - `USING zhparser`
///
/// Returns `Some(parser_name)` or `None` if no parser clause found.
fn extract_parser_name(rest: &str) -> Option<String> {
    let rest = rest.trim_start();

    // Pattern 1: USING <parser>
    if let Some(after_using) = consume_kw(rest, "USING") {
        let (name, _) = parse_identifier(after_using)?;
        // Strip schema prefix if present (e.g. pg_catalog.zhparser → zhparser).
        let bare = name.rsplit('.').next().unwrap_or(&name).to_string();
        return Some(bare);
    }

    // Pattern 2: ( PARSER = <parser> )
    if let Some(stripped) = rest.strip_prefix('(') {
        let inner = stripped.trim_start();
        let after_parser = consume_kw(inner, "PARSER")?;
        let after_eq = after_parser.trim_start().strip_prefix('=')?.trim_start();
        let (name, _) = parse_identifier(after_eq)?;
        let bare = name.rsplit('.').next().unwrap_or(&name).to_string();
        return Some(bare);
    }

    None
}

// ---------------------------------------------------------------------------
// Known zhparser-compatible parser names
// ---------------------------------------------------------------------------

/// Returns true if the parser name is zhparser or an equivalent we can map to
/// our built-in jieba tokenizer.
fn is_zhparser_compatible(parser_name: &str) -> bool {
    matches!(
        parser_name,
        "zhparser" | "jieba" | "chinese" | "zhparser_ngram" | "chinese_ngram"
    )
}

/// Given a zhparser-compatible parser name, return the tokenizer name to
/// persist in the config mapping.
fn resolve_tokenizer_for_parser(parser_name: &str) -> &str {
    match parser_name {
        "zhparser_ngram" | "chinese_ngram" => "chinese_ngram",
        // All other zhparser-compatible parsers map to "zhparser" (= jieba).
        _ => "zhparser",
    }
}

// ---------------------------------------------------------------------------
// Executor impls
// ---------------------------------------------------------------------------

impl Executor {
    /// Handle `CREATE TEXT SEARCH CONFIGURATION <name> (PARSER = <parser>)`.
    ///
    /// Only zhparser-compatible parsers are accepted.  The config name → tokenizer
    /// mapping is persisted per-tenant in TiKV so that `to_tsvector('name', ...)`
    /// can resolve it at query time.
    pub(crate) async fn execute_create_text_search_configuration_cmd(
        &self,
        session: &mut Session,
        sql: &str,
    ) -> Result<ExecuteResult> {
        let sql = strip_leading_sql_comments(sql);
        let sql = sql.trim().trim_end_matches(';').trim();

        // Skip "CREATE TEXT SEARCH CONFIGURATION".
        let rest = consume_kw(sql, "CREATE")
            .and_then(|r| consume_kw(r, "TEXT"))
            .and_then(|r| consume_kw(r, "SEARCH"))
            .and_then(|r| consume_kw(r, "CONFIGURATION"))
            .ok_or_else(|| anyhow!("syntax error in CREATE TEXT SEARCH CONFIGURATION"))?;

        // Parse config name.
        let (config_name, rest) =
            parse_identifier(rest).ok_or_else(|| anyhow!("missing configuration name"))?;

        // Extract parser name from options.
        let parser_name = extract_parser_name(rest)
            .ok_or_else(|| anyhow!("CREATE TEXT SEARCH CONFIGURATION requires PARSER option"))?;

        // Only zhparser-compatible parsers are accepted.
        if !is_zhparser_compatible(&parser_name) {
            return Err(SqlError::Unsupported(format!(
                "CREATE TEXT SEARCH CONFIGURATION with parser \"{}\" is not supported",
                parser_name
            ))
            .into());
        }

        let tokenizer_name = resolve_tokenizer_for_parser(&parser_name);

        // Check if the config name shadows a built-in config.
        if crate::sql::fts_tokenizers::get_tokenizer(&config_name).is_some() {
            return Err(anyhow!(
                "text search configuration \"{}\" already exists (built-in)",
                config_name
            ));
        }

        let is_autocommit = !session.is_in_transaction();
        if is_autocommit {
            session.begin().await?;
        }
        let db_id = session.current_database_id();
        let keyspace = self.tenant_keyspace().to_string();
        let result = async {
            let txn = session.get_mut_txn().expect("txn must be active");
            let store = self.store();
            if store
                .get_text_search_config(txn, db_id, &config_name)
                .await?
                .is_some()
            {
                return Err(anyhow!(
                    "text search configuration \"{}\" already exists",
                    config_name
                ));
            }
            store
                .put_text_search_config(txn, db_id, &config_name, tokenizer_name)
                .await?;
            info!(
                "CREATE TEXT SEARCH CONFIGURATION {}: parser={} -> tokenizer={}",
                config_name, parser_name, tokenizer_name
            );
            Ok(ExecuteResult::CommandComplete {
                tag: "CREATE TEXT SEARCH CONFIGURATION",
            })
        }
        .await;
        if is_autocommit {
            if result.is_ok() {
                session.commit().await?;
                crate::sql::fts_tokenizers::register_user_tsc(
                    &keyspace,
                    db_id,
                    &config_name,
                    tokenizer_name,
                );
            } else {
                session.rollback().await?;
            }
        } else if result.is_ok() {
            crate::sql::fts_tokenizers::register_user_tsc(
                &keyspace,
                db_id,
                &config_name,
                tokenizer_name,
            );
        }
        result
    }

    /// Handle `DROP TEXT SEARCH CONFIGURATION [IF EXISTS] <name>`.
    pub(crate) async fn execute_drop_text_search_configuration_cmd(
        &self,
        session: &mut Session,
        sql: &str,
    ) -> Result<ExecuteResult> {
        let sql = strip_leading_sql_comments(sql);
        let sql = sql.trim().trim_end_matches(';').trim();

        let rest = consume_kw(sql, "DROP")
            .and_then(|r| consume_kw(r, "TEXT"))
            .and_then(|r| consume_kw(r, "SEARCH"))
            .and_then(|r| consume_kw(r, "CONFIGURATION"))
            .ok_or_else(|| anyhow!("syntax error in DROP TEXT SEARCH CONFIGURATION"))?;

        let if_exists = starts_with_ignore_ascii_case(rest, "IF EXISTS");
        let rest = if if_exists {
            consume_kw(rest, "IF")
                .and_then(|r| consume_kw(r, "EXISTS"))
                .unwrap_or(rest)
        } else {
            rest
        };

        let (config_name, _) =
            parse_identifier(rest).ok_or_else(|| anyhow!("missing configuration name"))?;

        let is_autocommit = !session.is_in_transaction();
        if is_autocommit {
            session.begin().await?;
        }
        let db_id = session.current_database_id();
        let keyspace = self.tenant_keyspace().to_string();
        let result = async {
            let txn = session.get_mut_txn().expect("txn must be active");
            let store = self.store();
            let dropped = store
                .drop_text_search_config(txn, db_id, &config_name)
                .await?;
            if !dropped && !if_exists {
                return Err(anyhow!(
                    "text search configuration \"{}\" does not exist",
                    config_name
                ));
            }
            info!("DROP TEXT SEARCH CONFIGURATION {}", config_name);
            Ok(ExecuteResult::CommandComplete {
                tag: "DROP TEXT SEARCH CONFIGURATION",
            })
        }
        .await;
        if is_autocommit {
            if result.is_ok() {
                session.commit().await?;
                crate::sql::fts_tokenizers::unregister_user_tsc(&keyspace, db_id, &config_name);
            } else {
                session.rollback().await?;
            }
        } else if result.is_ok() {
            crate::sql::fts_tokenizers::unregister_user_tsc(&keyspace, db_id, &config_name);
        }
        result
    }

    /// Handle `ALTER TEXT SEARCH CONFIGURATION <name> ...`.
    ///
    /// Only `ADD MAPPING` / `ALTER MAPPING` / `DROP MAPPING` are recognized —
    /// all are no-ops for zhparser configs (the tokenizer mapping is implicit).
    /// Anything else returns `0A000`.
    pub(crate) async fn execute_alter_text_search_configuration_cmd(
        &self,
        session: &mut Session,
        sql: &str,
    ) -> Result<ExecuteResult> {
        let sql = strip_leading_sql_comments(sql);
        let sql_trimmed = sql.trim().trim_end_matches(';').trim();

        let rest = consume_kw(sql_trimmed, "ALTER")
            .and_then(|r| consume_kw(r, "TEXT"))
            .and_then(|r| consume_kw(r, "SEARCH"))
            .and_then(|r| consume_kw(r, "CONFIGURATION"))
            .ok_or_else(|| anyhow!("syntax error in ALTER TEXT SEARCH CONFIGURATION"))?;

        let (config_name, rest) =
            parse_identifier(rest).ok_or_else(|| anyhow!("missing configuration name"))?;

        let rest_upper = rest.trim().to_ascii_uppercase();

        // Accept ADD MAPPING, ALTER MAPPING, DROP MAPPING as no-ops.
        let is_mapping_op = rest_upper.starts_with("ADD MAPPING")
            || rest_upper.starts_with("ALTER MAPPING")
            || rest_upper.starts_with("DROP MAPPING");

        if !is_mapping_op {
            return Err(SqlError::Unsupported(format!(
                "ALTER TEXT SEARCH CONFIGURATION ... {} is not supported",
                rest.trim()
            ))
            .into());
        }

        // Verify the config exists (either built-in or stored).
        let config_exists = crate::sql::fts_tokenizers::get_tokenizer(&config_name).is_some();
        if !config_exists {
            // Check stored configs.
            let is_autocommit = !session.is_in_transaction();
            if is_autocommit {
                session.begin().await?;
            }
            let result = async {
                let db_id = session.current_database_id();
                let txn = session.get_mut_txn().expect("txn must be active");
                let stored = self
                    .store()
                    .get_text_search_config(txn, db_id, &config_name)
                    .await?;
                if stored.is_none() {
                    return Err(anyhow!(
                        "text search configuration \"{}\" does not exist",
                        config_name
                    ));
                }
                Ok(())
            }
            .await;
            if is_autocommit {
                if result.is_ok() {
                    session.commit().await?;
                } else {
                    session.rollback().await?;
                }
            }
            result?;
        }

        info!(
            "ALTER TEXT SEARCH CONFIGURATION {} (no-op mapping change)",
            config_name
        );

        Ok(ExecuteResult::CommandComplete {
            tag: "ALTER TEXT SEARCH CONFIGURATION",
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_consume_kw() {
        assert_eq!(consume_kw("CREATE foo", "CREATE"), Some("foo"));
        assert_eq!(consume_kw("create foo", "CREATE"), Some("foo"));
        assert_eq!(consume_kw("CREATES foo", "CREATE"), None);
        assert_eq!(consume_kw("  CREATE  foo", "CREATE"), Some("foo"));
    }

    #[test]
    fn test_parse_identifier_unquoted() {
        let (name, rest) = parse_identifier("zhcfg (PARSER = zhparser)").unwrap();
        assert_eq!(name, "zhcfg");
        assert!(rest.trim_start().starts_with('('));
    }

    #[test]
    fn test_parse_identifier_quoted() {
        let (name, rest) = parse_identifier("\"ZhCfg\" (PARSER = zhparser)").unwrap();
        assert_eq!(name, "zhcfg");
        assert!(rest.trim_start().starts_with('('));
    }

    #[test]
    fn test_extract_parser_using() {
        let parser = extract_parser_name("USING zhparser").unwrap();
        assert_eq!(parser, "zhparser");
    }

    #[test]
    fn test_extract_parser_using_schema_qualified() {
        let parser = extract_parser_name("USING pg_catalog.zhparser").unwrap();
        assert_eq!(parser, "zhparser");
    }

    #[test]
    fn test_extract_parser_paren() {
        let parser = extract_parser_name("( PARSER = zhparser )").unwrap();
        assert_eq!(parser, "zhparser");
    }

    #[test]
    fn test_extract_parser_paren_no_spaces() {
        let parser = extract_parser_name("(PARSER=zhparser)").unwrap();
        assert_eq!(parser, "zhparser");
    }

    #[test]
    fn test_is_zhparser_compatible() {
        assert!(is_zhparser_compatible("zhparser"));
        assert!(is_zhparser_compatible("jieba"));
        assert!(is_zhparser_compatible("chinese"));
        assert!(!is_zhparser_compatible("default"));
        assert!(!is_zhparser_compatible("pg_catalog.default"));
    }

    #[test]
    fn test_resolve_tokenizer() {
        assert_eq!(resolve_tokenizer_for_parser("zhparser"), "zhparser");
        assert_eq!(resolve_tokenizer_for_parser("chinese"), "zhparser");
        assert_eq!(
            resolve_tokenizer_for_parser("chinese_ngram"),
            "chinese_ngram"
        );
    }
}
