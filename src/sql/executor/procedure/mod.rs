//! Procedure and materialized view execution.
//!
//! SQL parsing helpers, parameter substitution, and re-exports for
//! `materialized_views` and `procedures` sub-modules.

mod materialized_views;
mod procedures;

use anyhow::{anyhow, Result};
use sqlparser::ast::{Ident, ObjectName};
use sqlparser::dialect::PostgreSqlDialect;
use sqlparser::tokenizer::{Token, Tokenizer};
use std::collections::HashMap;

// ── SQL parsing helpers ─────────────────────────────────────

pub(super) fn object_name_from_token(token: &str) -> Result<ObjectName> {
    let token = token.trim().trim_end_matches(';');
    if token.is_empty() {
        return Err(anyhow!("Missing object name"));
    }
    let parts: Vec<&str> = token.split('.').collect();
    match parts.as_slice() {
        [name] if !name.is_empty() => Ok(ObjectName(vec![sqlparser::ast::Ident::new(*name)])),
        [schema, name] if !schema.is_empty() && !name.is_empty() => Ok(ObjectName(vec![
            sqlparser::ast::Ident::new(*schema),
            sqlparser::ast::Ident::new(*name),
        ])),
        _ => Err(anyhow!("Invalid object name '{}'", token)),
    }
}

pub(super) fn tokenize_non_whitespace(sql: &str) -> Result<Vec<Token>> {
    let dialect = PostgreSqlDialect {};
    let mut tokenizer = Tokenizer::new(&dialect, sql);
    let tokens = tokenizer
        .tokenize()
        .map_err(|e| anyhow!("SQL tokenize error: {}", e))?;
    Ok(tokens
        .into_iter()
        .filter(|t| !matches!(t, Token::Whitespace(_)))
        .collect())
}

pub(super) fn is_unquoted_keyword(token: &Token, keyword: &str) -> bool {
    match token {
        Token::Word(w) => w.quote_style.is_none() && w.value.eq_ignore_ascii_case(keyword),
        _ => false,
    }
}

fn escape_sql_string(s: &str, quote_char: char) -> String {
    s.replace(quote_char, &format!("{}{}", quote_char, quote_char))
}

fn escape_postgresql_escaped_string(s: &str) -> String {
    // PostgreSQL E'...' strings need both backslashes and quotes escaped
    // Backslash: \ -> \\
    // Quote: ' -> ''
    s.replace('\\', "\\\\").replace('\'', "''")
}

pub(super) fn parse_call_arguments(args_str: &str) -> Result<Vec<String>> {
    if args_str.trim().is_empty() {
        return Ok(vec![]);
    }

    let dialect = PostgreSqlDialect {};
    let mut tokenizer = Tokenizer::new(&dialect, args_str);
    let tokens = tokenizer
        .tokenize()
        .map_err(|e| anyhow!("Failed to tokenize CALL arguments: {}", e))?;

    let mut args: Vec<String> = Vec::new();
    let mut current_arg = String::new();
    let mut depth = 0usize;

    for token in tokens {
        match token {
            Token::Comma if depth == 0 => {
                args.push(current_arg.trim().to_string());
                current_arg.clear();
            }
            Token::LParen => {
                depth += 1;
                current_arg.push('(');
            }
            Token::RParen => {
                if depth > 0 {
                    depth -= 1;
                }
                current_arg.push(')');
            }
            Token::LBracket => {
                depth += 1;
                current_arg.push('[');
            }
            Token::RBracket => {
                if depth > 0 {
                    depth -= 1;
                }
                current_arg.push(']');
            }
            Token::LBrace => {
                depth += 1;
                current_arg.push('{');
            }
            Token::RBrace => {
                if depth > 0 {
                    depth -= 1;
                }
                current_arg.push('}');
            }
            Token::Whitespace(ws) => {
                current_arg.push_str(&ws.to_string());
            }
            Token::Word(w) => {
                if let Some(q) = w.quote_style {
                    current_arg.push(q);
                    current_arg.push_str(&escape_sql_string(&w.value, q));
                    current_arg.push(q);
                } else {
                    current_arg.push_str(&w.value);
                }
            }
            Token::Number(n, _) => {
                current_arg.push_str(&n);
            }
            Token::SingleQuotedString(s) => {
                current_arg.push('\'');
                current_arg.push_str(&escape_sql_string(&s, '\''));
                current_arg.push('\'');
            }
            Token::DoubleQuotedString(s) => {
                current_arg.push('"');
                current_arg.push_str(&escape_sql_string(&s, '"'));
                current_arg.push('"');
            }
            Token::NationalStringLiteral(s) => {
                current_arg.push_str("N'");
                current_arg.push_str(&escape_sql_string(&s, '\''));
                current_arg.push('\'');
            }
            Token::HexStringLiteral(s) => {
                current_arg.push_str("X'");
                current_arg.push_str(&escape_sql_string(&s, '\''));
                current_arg.push('\'');
            }
            Token::EscapedStringLiteral(s) => {
                current_arg.push_str("E'");
                current_arg.push_str(&escape_postgresql_escaped_string(&s));
                current_arg.push('\'');
            }
            Token::Placeholder(s) => {
                current_arg.push_str(&s);
            }
            _ => {
                current_arg.push_str(&token.to_string());
            }
        }
    }

    if !current_arg.trim().is_empty() {
        args.push(current_arg.trim().to_string());
    }

    Ok(args)
}

pub(super) fn substitute_parameters_in_statement(
    stmt_str: &str,
    param_map: &HashMap<String, (String, String)>,
) -> Result<String> {
    let dialect = PostgreSqlDialect {};
    let mut tokenizer = Tokenizer::new(&dialect, stmt_str);
    let tokens = tokenizer
        .tokenize()
        .map_err(|e| anyhow!("Failed to tokenize statement: {}", e))?;

    let mut result = String::new();

    for token in tokens {
        match &token {
            Token::Word(w) if w.quote_style.is_none() => {
                if let Some((value, data_type)) = param_map.get(&w.value) {
                    let dt_lower = data_type.to_lowercase();
                    let formatted_value = if dt_lower.contains("int")
                        || dt_lower.contains("float")
                        || dt_lower.contains("real")
                        || dt_lower.contains("numeric")
                        || dt_lower.contains("decimal")
                        || dt_lower.contains("double")
                    {
                        value.clone()
                    } else if value.starts_with('\'') && value.ends_with('\'') {
                        value.clone()
                    } else {
                        format!("'{}'", value)
                    };
                    result.push_str(&formatted_value);
                } else {
                    result.push_str(&w.value);
                }
            }
            Token::Word(w) => {
                // Quoted identifier - must re-escape
                if let Some(q) = w.quote_style {
                    result.push(q);
                    result.push_str(&escape_sql_string(&w.value, q));
                    result.push(q);
                } else {
                    result.push_str(&w.value);
                }
            }
            Token::SingleQuotedString(s) => {
                result.push('\'');
                result.push_str(&escape_sql_string(s, '\''));
                result.push('\'');
            }
            Token::DoubleQuotedString(s) => {
                result.push('"');
                result.push_str(&escape_sql_string(s, '"'));
                result.push('"');
            }
            Token::NationalStringLiteral(s) => {
                result.push_str("N'");
                result.push_str(&escape_sql_string(s, '\''));
                result.push('\'');
            }
            Token::HexStringLiteral(s) => {
                result.push_str("X'");
                result.push_str(&escape_sql_string(s, '\''));
                result.push('\'');
            }
            Token::EscapedStringLiteral(s) => {
                result.push_str("E'");
                result.push_str(&escape_postgresql_escaped_string(s));
                result.push('\'');
            }
            _ => {
                result.push_str(&token.to_string());
            }
        }
    }

    Ok(result)
}

pub(super) fn parse_object_name(tokens: &[Token]) -> Result<(ObjectName, usize)> {
    let mut parts: Vec<Ident> = Vec::new();
    let mut i = 0usize;

    let Token::Word(w) = tokens
        .get(i)
        .ok_or_else(|| anyhow!("Missing object name"))?
    else {
        return Err(anyhow!("Missing object name"));
    };
    parts.push(Ident {
        value: w.value.clone(),
        quote_style: w.quote_style,
    });
    i += 1;

    if matches!(tokens.get(i), Some(Token::Period)) {
        i += 1;
        let Token::Word(w) = tokens
            .get(i)
            .ok_or_else(|| anyhow!("Invalid object name"))?
        else {
            return Err(anyhow!("Invalid object name"));
        };
        parts.push(Ident {
            value: w.value.clone(),
            quote_style: w.quote_style,
        });
        i += 1;
    }

    if matches!(tokens.get(i), Some(Token::Period)) {
        return Err(anyhow!("Invalid object name"));
    }

    Ok((ObjectName(parts), i))
}

#[cfg(test)]
mod tests {
    use super::materialized_views::{
        parse_drop_materialized_view, parse_refresh_materialized_view_name,
        DropMaterializedViewParsed,
    };
    use super::*;

    #[test]
    fn parse_refresh_materialized_view_preserves_quoted_ident_case() {
        let name =
            parse_refresh_materialized_view_name(r#"REFRESH MATERIALIZED VIEW "MyMV";"#).unwrap();
        assert_eq!(name.0.len(), 1);
        assert_eq!(name.0[0].value, "MyMV");
        assert_eq!(name.0[0].quote_style, Some('"'));
    }

    #[test]
    fn parse_drop_materialized_view_preserves_quoted_ident_case() {
        let DropMaterializedViewParsed {
            names,
            if_exists,
            cascade,
        } = parse_drop_materialized_view(r#"DROP MATERIALIZED VIEW IF EXISTS "MyMV";"#).unwrap();
        assert!(if_exists);
        assert!(!cascade);
        assert_eq!(names.len(), 1);
        assert_eq!(names[0].0.len(), 1);
        assert_eq!(names[0].0[0].value, "MyMV");
        assert_eq!(names[0].0[0].quote_style, Some('"'));
    }

    #[test]
    fn parse_refresh_materialized_view_supports_concurrently() {
        let name = parse_refresh_materialized_view_name(
            r#"REFRESH MATERIALIZED VIEW CONCURRENTLY public."MyMV";"#,
        )
        .unwrap();
        assert_eq!(name.0.len(), 2);
        assert_eq!(name.0[0].value, "public");
        assert_eq!(name.0[0].quote_style, None);
        assert_eq!(name.0[1].value, "MyMV");
        assert_eq!(name.0[1].quote_style, Some('"'));
    }

    #[test]
    fn parse_drop_materialized_view_supports_multiple_names() {
        let DropMaterializedViewParsed {
            names,
            if_exists,
            cascade: _cascade,
        } = parse_drop_materialized_view(
            r#"DROP MATERIALIZED VIEW IF EXISTS public."MyMV", "Other";"#,
        )
        .unwrap();
        assert!(if_exists);
        assert_eq!(names.len(), 2);
        assert_eq!(names[0].0.len(), 2);
        assert_eq!(names[0].0[1].value, "MyMV");
        assert_eq!(names[0].0[1].quote_style, Some('"'));
        assert_eq!(names[1].0.len(), 1);
        assert_eq!(names[1].0[0].value, "Other");
        assert_eq!(names[1].0[0].quote_style, Some('"'));
    }

    #[test]
    fn parse_drop_materialized_view_cascade() {
        let DropMaterializedViewParsed {
            names,
            if_exists,
            cascade,
        } = parse_drop_materialized_view(r#"DROP MATERIALIZED VIEW mv1 CASCADE;"#).unwrap();
        assert!(!if_exists);
        assert!(cascade);
        assert_eq!(names.len(), 1);
        assert_eq!(names[0].0[0].value, "mv1");
    }

    #[test]
    fn parse_call_arguments_handles_quoted_strings_with_commas() {
        let args = parse_call_arguments("'hello, world', 42").unwrap();
        assert_eq!(args.len(), 2);
        assert_eq!(args[0], "'hello, world'");
        assert_eq!(args[1], "42");
    }

    #[test]
    fn parse_call_arguments_handles_nested_parentheses() {
        let args = parse_call_arguments("func(1, 2), 'test'").unwrap();
        assert_eq!(args.len(), 2);
        assert_eq!(args[0], "func(1, 2)");
        assert_eq!(args[1], "'test'");
    }

    #[test]
    fn parse_call_arguments_handles_empty_string() {
        let args = parse_call_arguments("").unwrap();
        assert_eq!(args.len(), 0);
    }

    #[test]
    fn parse_call_arguments_handles_array_with_commas() {
        // Regression test: commas inside ARRAY[...] should not split arguments
        let args = parse_call_arguments("ARRAY[1,2], 3").unwrap();
        assert_eq!(args.len(), 2);
        assert_eq!(args[0], "ARRAY[1,2]");
        assert_eq!(args[1], "3");
    }

    #[test]
    fn parse_call_arguments_handles_braces_with_commas() {
        // Test curly braces nesting
        let args = parse_call_arguments("{1,2,3}, 'test'").unwrap();
        assert_eq!(args.len(), 2);
        assert_eq!(args[0], "{1,2,3}");
        assert_eq!(args[1], "'test'");
    }

    #[test]
    fn parse_call_arguments_handles_mixed_nesting() {
        // Test mixed parentheses, brackets, and braces
        let args = parse_call_arguments("func(ARRAY[1,2], {3,4}), 5").unwrap();
        assert_eq!(args.len(), 2);
        assert_eq!(args[0], "func(ARRAY[1,2], {3,4})");
        assert_eq!(args[1], "5");
    }

    #[test]
    fn substitute_parameters_only_replaces_unquoted_identifiers() {
        let mut param_map = HashMap::new();
        param_map.insert("id".to_string(), ("123".to_string(), "int".to_string()));

        let result = substitute_parameters_in_statement(
            "SELECT id, user_id, 'id' FROM users WHERE id = id",
            &param_map,
        )
        .unwrap();

        // Should replace unquoted 'id' but not 'user_id' or string literal 'id'
        assert!(result.contains("SELECT 123"));
        assert!(result.contains("user_id"));
        assert!(result.contains("'id'"));
        assert!(result.contains("WHERE 123 = 123"));
    }

    #[test]
    fn substitute_parameters_preserves_quoted_identifiers() {
        let mut param_map = HashMap::new();
        param_map.insert(
            "name".to_string(),
            ("'test'".to_string(), "text".to_string()),
        );

        let result =
            substitute_parameters_in_statement(r#"SELECT "name", name FROM users"#, &param_map)
                .unwrap();

        // Should not replace quoted identifier "name" but should replace unquoted name
        assert!(result.contains(r#""name""#));
        assert!(result.contains("'test'"));
    }

    #[test]
    fn parse_call_arguments_handles_escaped_string_literals_with_quotes() {
        // Test E'...' with embedded quote: E'it\'s fine'
        let args = parse_call_arguments(r"E'it\'s fine'").unwrap();
        assert_eq!(args.len(), 1);
        assert_eq!(args[0], r"E'it''s fine'");
    }

    #[test]
    fn parse_call_arguments_handles_escaped_string_literals_with_backslashes() {
        // Test E'...' with backslash: E'\\n' should remain as E'\\n' not E'\n'
        let args = parse_call_arguments(r"E'\\n'").unwrap();
        assert_eq!(args.len(), 1);
        assert_eq!(args[0], r"E'\\n'");
    }

    #[test]
    fn parse_call_arguments_handles_escaped_string_literals_mixed() {
        // Test E'...' with both backslashes and quotes
        let args = parse_call_arguments(r"E'path\\to\'file'").unwrap();
        assert_eq!(args.len(), 1);
        assert_eq!(args[0], r"E'path\\to''file'");
    }

    #[test]
    fn parse_call_arguments_handles_multiple_escaped_string_literals() {
        let args = parse_call_arguments(r"E'it\'s', E'\\test', 42").unwrap();
        assert_eq!(args.len(), 3);
        assert_eq!(args[0], r"E'it''s'");
        assert_eq!(args[1], r"E'\\test'");
        assert_eq!(args[2], "42");
    }

    #[test]
    fn substitute_parameters_handles_escaped_string_literals_with_quotes() {
        let param_map = HashMap::new();
        let result =
            substitute_parameters_in_statement(r"SELECT E'it\'s fine' FROM t", &param_map).unwrap();
        assert!(result.contains(r"E'it''s fine'"));
    }

    #[test]
    fn substitute_parameters_handles_escaped_string_literals_with_backslashes() {
        let param_map = HashMap::new();
        let result =
            substitute_parameters_in_statement(r"SELECT E'\\n' FROM t", &param_map).unwrap();
        assert!(result.contains(r"E'\\n'"));
    }

    #[test]
    fn substitute_parameters_handles_escaped_string_literals_mixed() {
        let param_map = HashMap::new();
        let result =
            substitute_parameters_in_statement(r"SELECT E'path\\to\'file' FROM t", &param_map)
                .unwrap();
        assert!(result.contains(r"E'path\\to''file'"));
    }
}
