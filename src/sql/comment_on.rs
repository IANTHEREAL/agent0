use super::helpers::normalize_ident;
use anyhow::{anyhow, Result};
use sqlparser::ast::{Ident, ObjectName};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum CommentOnTarget {
    Extension { name: String },
    Function { name: ObjectName },
    Table { name: ObjectName },
    Column { table: ObjectName, column: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CommentOnCommand {
    pub(crate) target: CommentOnTarget,
    /// `None` represents `IS NULL` (drop comment).
    pub(crate) comment: Option<String>,
}

fn skip_ws(input: &str, idx: &mut usize) {
    while let Some(ch) = input[*idx..].chars().next() {
        if ch.is_whitespace() {
            *idx += ch.len_utf8();
        } else {
            break;
        }
    }
}

fn is_ident_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_' || ch == '$'
}

fn consume_keyword(input: &str, idx: &mut usize, keyword: &str) -> bool {
    skip_ws(input, idx);
    let rest = &input[*idx..];
    if rest.len() < keyword.len() || !rest[..keyword.len()].eq_ignore_ascii_case(keyword) {
        return false;
    }
    let next = rest[keyword.len()..].chars().next();
    if matches!(next, Some(ch) if is_ident_char(ch)) {
        return false;
    }
    *idx += keyword.len();
    true
}

fn expect_keyword(input: &str, idx: &mut usize, keyword: &str) -> Result<()> {
    if consume_keyword(input, idx, keyword) {
        Ok(())
    } else {
        Err(anyhow!("Expected keyword '{}'", keyword))
    }
}

fn parse_ident(input: &str, idx: &mut usize) -> Result<Ident> {
    skip_ws(input, idx);
    let mut chars = input[*idx..].chars().peekable();
    let Some(ch) = chars.peek().copied() else {
        return Err(anyhow!("Expected identifier"));
    };

    if ch == '"' {
        chars.next();
        *idx += 1;
        let mut out = String::new();
        loop {
            let Some(next) = chars.next() else {
                return Err(anyhow!("Unterminated quoted identifier"));
            };
            *idx += next.len_utf8();
            match next {
                '"' => {
                    if chars.peek() == Some(&'"') {
                        chars.next();
                        *idx += 1;
                        out.push('"');
                    } else {
                        break;
                    }
                }
                other => out.push(other),
            }
        }
        return Ok(Ident::with_quote('"', out));
    }

    if !is_ident_char(ch) {
        return Err(anyhow!("Expected identifier"));
    }
    let start = *idx;
    while let Some(ch) = input[*idx..].chars().next() {
        if is_ident_char(ch) {
            *idx += ch.len_utf8();
        } else {
            break;
        }
    }
    Ok(Ident::new(&input[start..*idx]))
}

fn parse_object_name_max2(input: &str, idx: &mut usize) -> Result<ObjectName> {
    let first = parse_ident(input, idx)?;
    skip_ws(input, idx);
    if input[*idx..].starts_with('.') {
        *idx += 1;
        let second = parse_ident(input, idx)?;
        skip_ws(input, idx);
        if input[*idx..].starts_with('.') {
            return Err(anyhow!("Unsupported object name (too many parts)"));
        }
        Ok(ObjectName(vec![first, second]))
    } else {
        Ok(ObjectName(vec![first]))
    }
}

fn parse_column_target(input: &str, idx: &mut usize) -> Result<(ObjectName, String)> {
    let first = parse_ident(input, idx)?;
    skip_ws(input, idx);
    if !input[*idx..].starts_with('.') {
        return Err(anyhow!("COMMENT ON COLUMN target must be <table>.<column>"));
    }
    *idx += 1;
    let second = parse_ident(input, idx)?;
    skip_ws(input, idx);

    // Two-part form: table.column
    if !input[*idx..].starts_with('.') {
        let table = ObjectName(vec![first]);
        return Ok((table, normalize_ident(&second)));
    }

    // Three-part form: schema.table.column
    *idx += 1;
    let third = parse_ident(input, idx)?;
    skip_ws(input, idx);
    if input[*idx..].starts_with('.') {
        return Err(anyhow!(
            "COMMENT ON COLUMN target must be at most schema.table.column"
        ));
    }

    Ok((ObjectName(vec![first, second]), normalize_ident(&third)))
}

fn skip_balanced_parens(input: &str, idx: &mut usize) -> Result<()> {
    skip_ws(input, idx);
    if !input[*idx..].starts_with('(') {
        return Ok(());
    }

    let mut depth: i32 = 0;
    let mut in_single = false;
    let mut in_double = false;

    while let Some(ch) = input[*idx..].chars().next() {
        *idx += ch.len_utf8();
        if in_single {
            if ch == '\'' {
                if input[*idx..].starts_with('\'') {
                    *idx += 1;
                } else {
                    in_single = false;
                }
            }
            continue;
        }
        if in_double {
            if ch == '"' {
                if input[*idx..].starts_with('"') {
                    *idx += 1;
                } else {
                    in_double = false;
                }
            }
            continue;
        }

        match ch {
            '\'' => in_single = true,
            '"' => in_double = true,
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Ok(());
                }
                if depth < 0 {
                    return Err(anyhow!("Unbalanced parentheses"));
                }
            }
            _ => {}
        }
    }

    Err(anyhow!("Unbalanced parentheses"))
}

fn parse_sql_string_or_dollar_literal(input: &str, idx: &mut usize) -> Result<String> {
    skip_ws(input, idx);
    let s = &input[*idx..];
    if s.is_empty() {
        return Err(anyhow!("Expected string literal"));
    }

    if s.starts_with('$') {
        let bytes = s.as_bytes();
        let Some(end) = bytes[1..].iter().position(|&c| c == b'$') else {
            return Err(anyhow!("Invalid dollar-quoted string"));
        };
        let delim_end = 1 + end;
        let tag = &bytes[1..delim_end];
        if !tag.iter().all(|c| c.is_ascii_alphanumeric() || *c == b'_') {
            return Err(anyhow!("Invalid dollar-quote tag"));
        }
        let delim = &s[..=delim_end];
        let body_start = delim_end + 1;
        let Some(close_rel) = s[body_start..].find(delim) else {
            return Err(anyhow!("Unterminated dollar-quoted string"));
        };
        let body_end = body_start + close_rel;
        *idx += body_end + delim.len();
        return Ok(s[body_start..body_end].to_string());
    }

    if s.starts_with('\'') {
        let bytes = s.as_bytes();
        let mut out = String::new();
        let mut i = 1usize;
        while i < bytes.len() {
            let b = bytes[i];
            if b == b'\'' {
                if i + 1 < bytes.len() && bytes[i + 1] == b'\'' {
                    out.push('\'');
                    i += 2;
                    continue;
                }
                *idx += i + 1;
                return Ok(out);
            }
            out.push(b as char);
            i += 1;
        }
        return Err(anyhow!("Unterminated string literal"));
    }

    Err(anyhow!("Expected string literal"))
}

fn parse_comment_value(input: &str, idx: &mut usize) -> Result<Option<String>> {
    if consume_keyword(input, idx, "NULL") {
        return Ok(None);
    }
    Ok(Some(parse_sql_string_or_dollar_literal(input, idx)?))
}

pub(crate) fn parse_comment_on_sql(sql: &str) -> Result<CommentOnCommand> {
    let sql = super::executor::triggers::strip_leading_sql_comments(sql).trim();
    let sql = sql.trim_end_matches(';').trim_end();

    let mut idx = 0usize;
    expect_keyword(sql, &mut idx, "COMMENT")?;
    expect_keyword(sql, &mut idx, "ON")?;

    let target = if consume_keyword(sql, &mut idx, "EXTENSION") {
        let ident = parse_ident(sql, &mut idx)?;
        CommentOnTarget::Extension {
            name: normalize_ident(&ident).to_lowercase(),
        }
    } else if consume_keyword(sql, &mut idx, "FUNCTION") {
        let name = parse_object_name_max2(sql, &mut idx)?;
        skip_balanced_parens(sql, &mut idx)?;
        CommentOnTarget::Function { name }
    } else if consume_keyword(sql, &mut idx, "TABLE") {
        let name = parse_object_name_max2(sql, &mut idx)?;
        CommentOnTarget::Table { name }
    } else if consume_keyword(sql, &mut idx, "COLUMN") {
        let (table, column) = parse_column_target(sql, &mut idx)?;
        CommentOnTarget::Column { table, column }
    } else {
        return Err(anyhow!("Unsupported COMMENT ON statement"));
    };

    expect_keyword(sql, &mut idx, "IS")?;
    let comment = parse_comment_value(sql, &mut idx)?;

    Ok(CommentOnCommand { target, comment })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_comment_on_extension() {
        let cmd = parse_comment_on_sql("COMMENT ON EXTENSION \"uuid-ossp\" IS 'x';").unwrap();
        assert_eq!(
            cmd,
            CommentOnCommand {
                target: CommentOnTarget::Extension {
                    name: "uuid-ossp".to_string()
                },
                comment: Some("x".to_string())
            }
        );
    }

    #[test]
    fn parse_comment_on_function() {
        let cmd = parse_comment_on_sql("COMMENT ON FUNCTION public.uuidv7() IS 'x';").unwrap();
        let CommentOnTarget::Function { name } = cmd.target else {
            panic!("expected function target");
        };
        assert_eq!(name.0.len(), 2);
        assert_eq!(cmd.comment.as_deref(), Some("x"));
    }

    #[test]
    fn parse_comment_on_column_schema_table_column() {
        let cmd = parse_comment_on_sql("COMMENT ON COLUMN public.t.c IS 'x';").unwrap();
        let CommentOnTarget::Column { table, column } = cmd.target else {
            panic!("expected column target");
        };
        assert_eq!(table.0.len(), 2);
        assert_eq!(normalize_ident(&table.0[0]), "public");
        assert_eq!(normalize_ident(&table.0[1]), "t");
        assert_eq!(column, "c");
        assert_eq!(cmd.comment.as_deref(), Some("x"));
    }

    #[test]
    fn parse_comment_on_column_table_column_null() {
        let cmd = parse_comment_on_sql("COMMENT ON COLUMN t.c IS NULL;").unwrap();
        let CommentOnTarget::Column { table, column } = cmd.target else {
            panic!("expected column target");
        };
        assert_eq!(table.0.len(), 1);
        assert_eq!(normalize_ident(&table.0[0]), "t");
        assert_eq!(column, "c");
        assert!(cmd.comment.is_none());
    }

    #[test]
    fn parse_comment_string_with_escaped_quote() {
        let cmd = parse_comment_on_sql("COMMENT ON EXTENSION \"uuid-ossp\" IS 'a''b';").unwrap();
        assert_eq!(cmd.comment.as_deref(), Some("a'b"));
    }
}

