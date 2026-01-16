use anyhow::{anyhow, Result};
use tikv_client::Transaction;

use super::executor::Executor;
use super::udt;
use super::{ExecuteResult, Session};

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
    if next.is_whitespace() || matches!(next, '(' | ')' | ',' | ';') {
        return Some(rest);
    }
    None
}

fn next_token(input: &str) -> Option<(&str, &str)> {
    let s = input.trim_start();
    if s.is_empty() {
        return None;
    }

    for (idx, ch) in s.char_indices() {
        if ch.is_whitespace() {
            return Some((&s[..idx], &s[idx..]));
        }
    }
    Some((s, ""))
}

fn parse_type_name_token(token: &str) -> Result<(String, String, String)> {
    fn push_part(parts: &mut Vec<String>, raw: &str, quoted: bool) -> Result<()> {
        let trimmed = if quoted { raw } else { raw.trim() };
        if trimmed.is_empty() {
            return Err(anyhow!("Invalid type name"));
        }
        if quoted {
            parts.push(trimmed.to_string());
        } else {
            parts.push(trimmed.to_lowercase());
        }
        Ok(())
    }

    let mut parts: Vec<String> = Vec::new();
    let mut buf = String::new();
    let mut in_quotes = false;
    let mut part_quoted = false;

    let mut chars = token.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '"' => {
                if in_quotes {
                    if chars.peek() == Some(&'"') {
                        chars.next();
                        buf.push('"');
                    } else {
                        in_quotes = false;
                    }
                } else {
                    in_quotes = true;
                    part_quoted = true;
                }
            }
            '.' if !in_quotes => {
                push_part(&mut parts, &buf, part_quoted)?;
                buf.clear();
                part_quoted = false;
            }
            _ => buf.push(ch),
        }
    }

    if in_quotes {
        return Err(anyhow!("Unterminated quoted identifier in type name"));
    }
    push_part(&mut parts, &buf, part_quoted)?;

    let (schema, name) = if parts.len() >= 2 {
        (
            parts[parts.len() - 2].clone(),
            parts[parts.len() - 1].clone(),
        )
    } else {
        ("public".to_string(), parts[0].clone())
    };

    Ok((schema.clone(), name.clone(), format!("{}.{}", schema, name)))
}

fn parse_enum_labels(input: &str) -> Result<(Vec<String>, &str)> {
    let mut i: usize = 0;
    let input = input.trim_start();
    if !input.starts_with('(') {
        return Err(anyhow!("Expected '(' after AS ENUM"));
    }
    i += 1;

    let mut labels = Vec::new();
    loop {
        // Skip whitespace and commas
        while i < input.len() {
            let ch = input[i..].chars().next().unwrap();
            if ch.is_whitespace() || ch == ',' {
                i += ch.len_utf8();
            } else {
                break;
            }
        }

        if i >= input.len() {
            return Err(anyhow!("Unterminated ENUM label list"));
        }

        let ch = input[i..].chars().next().unwrap();
        if ch == ')' {
            i += 1;
            break;
        }

        if ch != '\'' {
            return Err(anyhow!("Expected string literal for enum label"));
        }
        i += 1;

        let mut label = String::new();
        loop {
            if i >= input.len() {
                return Err(anyhow!("Unterminated string literal in enum label list"));
            }

            let ch = input[i..].chars().next().unwrap();
            if ch == '\'' {
                if input.as_bytes().get(i + 1) == Some(&b'\'') {
                    label.push('\'');
                    i += 2;
                    continue;
                }
                i += 1;
                break;
            }

            label.push(ch);
            i += ch.len_utf8();
        }

        labels.push(label);

        // Skip whitespace
        while i < input.len() {
            let ch = input[i..].chars().next().unwrap();
            if ch.is_whitespace() {
                i += ch.len_utf8();
            } else {
                break;
            }
        }

        if i >= input.len() {
            return Err(anyhow!("Unterminated ENUM label list"));
        }

        let ch = input[i..].chars().next().unwrap();
        if ch == ',' {
            i += 1;
            continue;
        }
        if ch == ')' {
            i += 1;
            break;
        }

        return Err(anyhow!("Expected ',' or ')' after enum label"));
    }

    Ok((labels, &input[i..]))
}

fn strip_trailing_drop_behavior(input: &str) -> &str {
    let trimmed = input.trim_end();
    let tokens: Vec<&str> = trimmed.split_whitespace().collect();
    let Some(last) = tokens.last() else {
        return trimmed;
    };

    if !last.eq_ignore_ascii_case("CASCADE") && !last.eq_ignore_ascii_case("RESTRICT") {
        return trimmed;
    }

    let Some(pos) = trimmed.rfind(last) else {
        return trimmed;
    };
    trimmed[..pos].trim_end()
}

fn split_names_list(input: &str) -> Result<Vec<&str>> {
    let mut parts = Vec::new();
    let mut start = 0usize;
    let mut in_quotes = false;
    let mut chars = input.char_indices().peekable();

    while let Some((idx, ch)) = chars.next() {
        match ch {
            '"' => {
                if in_quotes && chars.peek().map(|(_, c)| *c == '"').unwrap_or(false) {
                    chars.next();
                } else {
                    in_quotes = !in_quotes;
                }
            }
            ',' if !in_quotes => {
                parts.push(input[start..idx].trim());
                start = idx + 1;
            }
            _ => {}
        }
    }
    parts.push(input[start..].trim());

    if parts.iter().any(|p| p.is_empty()) {
        return Err(anyhow!("Invalid type name list"));
    }

    Ok(parts)
}

impl Executor {
    pub(crate) async fn execute_create_type_enum_cmd(
        &self,
        session: &mut Session,
        sql: &str,
    ) -> Result<ExecuteResult> {
        let sql = trim_sql_end(sql);
        let mut rest =
            consume_keyword(sql, "CREATE").ok_or_else(|| anyhow!("Invalid CREATE TYPE syntax"))?;
        rest =
            consume_keyword(rest, "TYPE").ok_or_else(|| anyhow!("Invalid CREATE TYPE syntax"))?;

        let (type_token, rest) = next_token(rest).ok_or_else(|| anyhow!("Missing type name"))?;
        let mut rest = rest;

        rest = consume_keyword(rest, "AS")
            .ok_or_else(|| anyhow!("Invalid CREATE TYPE AS ENUM syntax"))?;
        rest = consume_keyword(rest, "ENUM")
            .ok_or_else(|| anyhow!("Invalid CREATE TYPE AS ENUM syntax"))?;

        let (schema, name, _full_name) = parse_type_name_token(type_token)?;
        let (labels, _tail) = parse_enum_labels(rest)?;

        let is_autocommit = !session.is_in_transaction();
        if is_autocommit {
            session.begin().await?;
        }

        let result = async {
            let txn: &mut Transaction = session.get_mut_txn().expect("Transaction must be active");
            udt::create_enum_type(&self.store(), txn, schema, name, labels).await
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

    pub(crate) async fn execute_drop_type_cmd(
        &self,
        session: &mut Session,
        sql: &str,
    ) -> Result<ExecuteResult> {
        let sql = trim_sql_end(sql);
        let mut rest =
            consume_keyword(sql, "DROP").ok_or_else(|| anyhow!("Invalid DROP TYPE syntax"))?;
        rest = consume_keyword(rest, "TYPE").ok_or_else(|| anyhow!("Invalid DROP TYPE syntax"))?;

        let rest = rest.trim_start();
        let (if_exists, names_part) = if let Some(after_if) = consume_keyword(rest, "IF") {
            if let Some(after_exists) = consume_keyword(after_if, "EXISTS") {
                (true, after_exists)
            } else {
                (false, rest)
            }
        } else {
            (false, rest)
        };

        let names_part = strip_trailing_drop_behavior(names_part);
        let raw_names = split_names_list(names_part)?;
        let mut full_names = Vec::with_capacity(raw_names.len());
        for raw in raw_names {
            let (schema, name, full_name) = parse_type_name_token(raw)?;
            let _ = schema;
            let _ = name;
            full_names.push(full_name);
        }

        let is_autocommit = !session.is_in_transaction();
        if is_autocommit {
            session.begin().await?;
        }

        let result = async {
            let txn: &mut Transaction = session.get_mut_txn().expect("Transaction must be active");
            udt::drop_types(&self.store(), txn, &full_names, if_exists).await
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
    fn test_consume_keyword_accepts_paren_boundary() {
        assert_eq!(consume_keyword("ENUM('USER')", "ENUM"), Some("('USER')"));
        assert_eq!(consume_keyword("ENUM ('USER')", "ENUM"), Some(" ('USER')"));
        assert_eq!(consume_keyword("ENUMX('USER')", "ENUM"), None);
    }

    #[test]
    fn test_parse_type_name_token_default_schema_lowercase() {
        let (schema, name, full) = parse_type_name_token("Role").unwrap();
        assert_eq!(schema, "public");
        assert_eq!(name, "role");
        assert_eq!(full, "public.role");
    }

    #[test]
    fn test_parse_type_name_token_quoted_preserves_case() {
        let (schema, name, full) = parse_type_name_token("\"Role\"").unwrap();
        assert_eq!(schema, "public");
        assert_eq!(name, "Role");
        assert_eq!(full, "public.Role");
    }

    #[test]
    fn test_parse_type_name_token_schema_qualified() {
        let (schema, name, full) = parse_type_name_token("MySchema.Role").unwrap();
        assert_eq!(schema, "myschema");
        assert_eq!(name, "role");
        assert_eq!(full, "myschema.role");

        let (schema, name, full) = parse_type_name_token("\"MySchema\".\"Role\"").unwrap();
        assert_eq!(schema, "MySchema");
        assert_eq!(name, "Role");
        assert_eq!(full, "MySchema.Role");
    }

    #[test]
    fn test_parse_enum_labels_basic_and_escaped_quote() {
        let (labels, tail) = parse_enum_labels("('USER','ADMIN')").unwrap();
        assert_eq!(labels, vec!["USER".to_string(), "ADMIN".to_string()]);
        assert_eq!(tail, "");

        let (labels, tail) = parse_enum_labels("('O''Reilly')").unwrap();
        assert_eq!(labels, vec!["O'Reilly".to_string()]);
        assert_eq!(tail, "");
    }

    #[test]
    fn test_strip_trailing_drop_behavior_and_name_list() {
        assert_eq!(strip_trailing_drop_behavior("role CASCADE"), "role");
        assert_eq!(strip_trailing_drop_behavior("role RESTRICT"), "role");
        assert_eq!(strip_trailing_drop_behavior("role"), "role");
        assert_eq!(
            strip_trailing_drop_behavior("role, comp CASCADE"),
            "role, comp"
        );

        let names = split_names_list("role, \"Role\", myschema.role").unwrap();
        assert_eq!(names, vec!["role", "\"Role\"", "myschema.role"]);
    }
}
