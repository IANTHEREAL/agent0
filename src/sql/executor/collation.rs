//! CREATE COLLATION and DROP COLLATION executor handlers

use anyhow::{anyhow, Result};

use crate::sql::error::SqlError;

use super::super::collation::CollationDef;
use super::super::{ExecuteResult, Session};
use super::core::Executor;

fn trim_sql_end(sql: &str) -> &str {
    sql.trim()
        .trim_end_matches(';')
        .trim_end_matches(|c: char| c.is_whitespace())
}

fn consume_keyword<'a>(input: &'a str, keyword: &str) -> Option<&'a str> {
    let input = input.trim_start();
    if input.len() < keyword.len() {
        return None;
    }

    let (head, rest) = input.split_at(keyword.len());
    if !head.eq_ignore_ascii_case(keyword) {
        return None;
    }

    if rest.is_empty() {
        return Some(rest);
    }
    let next = rest.chars().next()?;
    if next.is_whitespace() || matches!(next, '(' | ')' | ',' | ';' | '=') {
        return Some(rest);
    }
    None
}

/// Parse a potentially quoted identifier
fn parse_identifier(input: &str) -> Result<(String, &str)> {
    let input = input.trim_start();
    if input.is_empty() {
        return Err(anyhow!("Expected identifier"));
    }

    if input.starts_with('"') {
        // Quoted identifier
        let mut i = 1;
        let mut name = String::new();
        while i < input.len() {
            let ch = input.chars().nth(i).unwrap();
            if ch == '"' {
                if input.chars().nth(i + 1) == Some('"') {
                    name.push('"');
                    i += 2;
                } else {
                    i += 1;
                    return Ok((name, &input[i..]));
                }
            } else {
                name.push(ch);
                i += ch.len_utf8();
            }
        }
        return Err(anyhow!("Unterminated quoted identifier"));
    }

    // Unquoted identifier — lowercase per SQL standard (matches normalize_ident)
    let end = input
        .find(|c: char| !c.is_alphanumeric() && c != '_')
        .unwrap_or(input.len());
    if end == 0 {
        return Err(anyhow!("Invalid identifier"));
    }
    Ok((input[..end].to_lowercase(), &input[end..]))
}

/// Parse a quoted string literal
fn parse_string_literal(input: &str) -> Result<(String, &str)> {
    let input = input.trim_start();
    if input.is_empty() || !input.starts_with('\'') {
        return Err(anyhow!("Expected string literal"));
    }

    let mut i = 1;
    let mut value = String::new();
    while i < input.len() {
        let ch = input.chars().nth(i).unwrap();
        if ch == '\'' {
            if input.chars().nth(i + 1) == Some('\'') {
                value.push('\'');
                i += 2;
            } else {
                i += 1;
                return Ok((value, &input[i..]));
            }
        } else {
            value.push(ch);
            i += ch.len_utf8();
        }
    }
    Err(anyhow!("Unterminated string literal"))
}

/// Parse CREATE COLLATION statement. Returns (CollationDef, if_not_exists).
fn parse_create_collation(sql: &str) -> Result<(CollationDef, bool)> {
    let sql = trim_sql_end(sql);

    let rest = consume_keyword(sql, "CREATE").ok_or_else(|| anyhow!("Expected CREATE keyword"))?;
    let rest =
        consume_keyword(rest, "COLLATION").ok_or_else(|| anyhow!("Expected COLLATION keyword"))?;

    // Parse optional IF NOT EXISTS
    let (rest, if_not_exists) = if let Some(rest_after_if) = consume_keyword(rest, "IF") {
        let rest_after_not = consume_keyword(rest_after_if, "NOT")
            .ok_or_else(|| anyhow!("Expected NOT after IF"))?;
        let rest_after_exists = consume_keyword(rest_after_not, "EXISTS")
            .ok_or_else(|| anyhow!("Expected EXISTS after NOT"))?;
        (rest_after_exists, true)
    } else {
        (rest, false)
    };

    // Parse collation name
    let (name, rest) = parse_identifier(rest)?;

    // Parse options list in parentheses
    let rest = rest.trim_start();
    if !rest.starts_with('(') {
        return Err(anyhow!("Expected '(' after collation name"));
    }
    let rest = &rest[1..];

    let mut provider = String::from("icu");
    let mut locale: Option<String> = None;
    let mut deterministic = true;

    // Parse options
    let mut rest = rest;
    loop {
        rest = rest.trim_start();
        if rest.starts_with(')') {
            break;
        }

        // Parse option name
        let (opt_name, rest_after_name) = parse_identifier(rest)?;
        rest = rest_after_name.trim_start();

        // Expect '='
        if !rest.starts_with('=') {
            return Err(anyhow!("Expected '=' after option name"));
        }
        rest = rest[1..].trim_start();

        // Parse option value (string literal or identifier)
        let opt_value = if rest.starts_with('\'') {
            let (val, rest_after_val) = parse_string_literal(rest)?;
            rest = rest_after_val;
            val
        } else {
            let (val, rest_after_val) = parse_identifier(rest)?;
            rest = rest_after_val;
            val
        };

        // Process option
        match opt_name.to_lowercase().as_str() {
            "provider" => provider = opt_value.to_lowercase(),
            "locale" => locale = Some(opt_value),
            "deterministic" => {
                deterministic = match opt_value.to_lowercase().as_str() {
                    "true" | "yes" | "on" => true,
                    "false" | "no" | "off" => false,
                    _ => return Err(anyhow!("Invalid value for deterministic option")),
                };
            }
            _ => return Err(anyhow!("Unknown collation option: {}", opt_name)),
        }

        // Skip comma if present
        rest = rest.trim_start();
        if rest.starts_with(',') {
            rest = &rest[1..];
        }
    }

    Ok((
        CollationDef {
            name,
            provider,
            locale,
            deterministic,
        },
        if_not_exists,
    ))
}

/// Parse DROP COLLATION statement
fn parse_drop_collation(sql: &str) -> Result<(String, bool)> {
    let sql = trim_sql_end(sql);

    let rest = consume_keyword(sql, "DROP").ok_or_else(|| anyhow!("Expected DROP keyword"))?;
    let rest =
        consume_keyword(rest, "COLLATION").ok_or_else(|| anyhow!("Expected COLLATION keyword"))?;

    // Parse optional IF EXISTS
    let (rest, if_exists) = if let Some(rest_after_if) = consume_keyword(rest, "IF") {
        let rest_after_exists = consume_keyword(rest_after_if, "EXISTS")
            .ok_or_else(|| anyhow!("Expected EXISTS after IF"))?;
        (rest_after_exists, true)
    } else {
        (rest, false)
    };

    // Parse collation name
    let (name, _rest) = parse_identifier(rest)?;

    Ok((name, if_exists))
}

impl Executor {
    pub(crate) async fn execute_create_collation_cmd(
        &self,
        session: &mut Session,
        sql: &str,
    ) -> Result<ExecuteResult> {
        let (collation_def, if_not_exists) = parse_create_collation(sql)?;

        // Validate provider + locale before touching the transaction
        if !matches!(collation_def.provider.as_str(), "icu" | "c" | "d") {
            return Err(anyhow!(
                "invalid collation provider: {}",
                collation_def.provider
            ));
        }
        if collation_def.provider == "icu" && collation_def.locale.is_none() {
            return Err(anyhow!("ICU collation requires a locale"));
        }

        // Built-in collision check — c/posix/default are not in TiKV.
        // Must be before session.begin() to avoid leaking a transaction.
        let name_lower = collation_def.name.to_lowercase();
        if matches!(name_lower.as_str(), "c" | "posix" | "default") {
            if if_not_exists {
                return Ok(ExecuteResult::CommandComplete {
                    tag: "CREATE COLLATION",
                });
            }
            return Err(anyhow!(
                "collation \"{}\" already exists",
                collation_def.name
            ));
        }

        let is_autocommit = !session.is_in_transaction();
        if is_autocommit {
            session.begin().await?;
        }

        let result = async {
            let db_id = session.current_database_id();
            let txn = session.get_mut_txn().expect("txn must be active");
            let store = self.store();
            match store.create_collation(txn, db_id, &collation_def).await {
                Ok(()) => Ok(ExecuteResult::CommandComplete {
                    tag: "CREATE COLLATION",
                }),
                Err(e) => {
                    if if_not_exists
                        && e.downcast_ref::<SqlError>()
                            .is_some_and(|se| matches!(se, SqlError::DuplicateObject(_)))
                    {
                        Ok(ExecuteResult::CommandComplete {
                            tag: "CREATE COLLATION",
                        })
                    } else {
                        Err(e)
                    }
                }
            }
        }
        .await;

        if is_autocommit {
            if result.is_ok() {
                session.commit().await?;
            } else {
                session.rollback().await?;
            }
        }

        result
    }

    pub(crate) async fn execute_drop_collation_cmd(
        &self,
        session: &mut Session,
        sql: &str,
    ) -> Result<ExecuteResult> {
        let (name, if_exists) = parse_drop_collation(sql)?;

        // Built-in collations cannot be dropped.
        // Must be before session.begin() to avoid leaking a transaction.
        let name_lower = name.to_lowercase();
        if matches!(name_lower.as_str(), "c" | "posix" | "default") {
            return Err(anyhow!("cannot drop built-in collation \"{}\"", name));
        }

        let is_autocommit = !session.is_in_transaction();
        if is_autocommit {
            session.begin().await?;
        }

        let result = async {
            let db_id = session.current_database_id();
            let txn = session.get_mut_txn().expect("txn must be active");
            let store = self.store();
            let found = store.drop_collation(txn, db_id, &name).await?;
            if !found {
                if if_exists {
                    return Ok(ExecuteResult::CommandComplete {
                        tag: "DROP COLLATION",
                    });
                }
                return Err(anyhow!("collation \"{}\" does not exist", name));
            }
            Ok(ExecuteResult::CommandComplete {
                tag: "DROP COLLATION",
            })
        }
        .await;

        if is_autocommit {
            if result.is_ok() {
                session.commit().await?;
            } else {
                session.rollback().await?;
            }
        }

        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_create_collation() {
        let sql = "CREATE COLLATION da (provider = icu, locale = 'da-u-kf-lower')";
        let (def, if_not_exists) = parse_create_collation(sql).unwrap();
        assert_eq!(def.name, "da");
        assert_eq!(def.provider, "icu");
        assert_eq!(def.locale, Some("da-u-kf-lower".to_string()));
        assert!(def.deterministic);
        assert!(!if_not_exists);
    }

    #[test]
    fn test_parse_create_collation_quoted() {
        let sql = "CREATE COLLATION \"my collation\" (provider = 'icu', locale = 'de')";
        let (def, if_not_exists) = parse_create_collation(sql).unwrap();
        assert_eq!(def.name, "my collation");
        assert_eq!(def.provider, "icu");
        assert_eq!(def.locale, Some("de".to_string()));
        assert!(!if_not_exists);
    }

    #[test]
    fn test_parse_create_collation_if_not_exists() {
        let sql = "CREATE COLLATION IF NOT EXISTS da (provider = icu, locale = 'da-u-kf-lower')";
        let (def, if_not_exists) = parse_create_collation(sql).unwrap();
        assert_eq!(def.name, "da");
        assert!(if_not_exists);
    }

    #[test]
    fn test_parse_identifier_lowercases_unquoted() {
        let (name, _) = parse_identifier("DA rest").unwrap();
        assert_eq!(name, "da");
    }

    #[test]
    fn test_parse_identifier_preserves_quoted() {
        let (name, _) = parse_identifier("\"DA\" rest").unwrap();
        assert_eq!(name, "DA");
    }

    #[test]
    fn test_parse_drop_collation() {
        let sql = "DROP COLLATION da";
        let (name, if_exists) = parse_drop_collation(sql).unwrap();
        assert_eq!(name, "da");
        assert!(!if_exists);
    }

    #[test]
    fn test_parse_drop_collation_if_exists() {
        let sql = "DROP COLLATION IF EXISTS da";
        let (name, if_exists) = parse_drop_collation(sql).unwrap();
        assert_eq!(name, "da");
        assert!(if_exists);
    }
}
