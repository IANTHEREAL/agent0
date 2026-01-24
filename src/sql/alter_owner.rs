use super::helpers::normalize_ident;
use anyhow::{anyhow, Result};
use sqlparser::ast::{Ident, ObjectName};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AlterOwnerKind {
    Table,
    Sequence,
    Function,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AlterOwnerCommand {
    pub(crate) kind: AlterOwnerKind,
    pub(crate) if_exists: bool,
    pub(crate) name: ObjectName,
    pub(crate) new_owner: String,
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

fn parse_object_name(input: &str, idx: &mut usize) -> Result<ObjectName> {
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
                    // Escaped single quote: ''.
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

pub(crate) fn parse_alter_owner_sql(sql: &str) -> Result<AlterOwnerCommand> {
    let sql = super::executor::triggers::strip_leading_sql_comments(sql).trim();
    let sql = sql.trim_end_matches(';').trim_end();

    let mut idx = 0usize;
    expect_keyword(sql, &mut idx, "ALTER")?;

    let kind = if consume_keyword(sql, &mut idx, "TABLE") {
        AlterOwnerKind::Table
    } else if consume_keyword(sql, &mut idx, "SEQUENCE") {
        AlterOwnerKind::Sequence
    } else if consume_keyword(sql, &mut idx, "FUNCTION") {
        AlterOwnerKind::Function
    } else {
        return Err(anyhow!("Unsupported ALTER ... OWNER TO statement"));
    };

    let if_exists = if consume_keyword(sql, &mut idx, "IF") {
        expect_keyword(sql, &mut idx, "EXISTS")?;
        true
    } else {
        false
    };

    if kind == AlterOwnerKind::Table {
        let _ = consume_keyword(sql, &mut idx, "ONLY");
    }

    let name = parse_object_name(sql, &mut idx)?;

    if kind == AlterOwnerKind::Function {
        skip_balanced_parens(sql, &mut idx)?;
    }

    expect_keyword(sql, &mut idx, "OWNER")?;
    expect_keyword(sql, &mut idx, "TO")?;
    let owner_ident = parse_ident(sql, &mut idx)?;
    let new_owner = normalize_ident(&owner_ident);

    Ok(AlterOwnerCommand {
        kind,
        if_exists,
        name,
        new_owner,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_alter_table_owner() {
        let cmd =
            parse_alter_owner_sql("ALTER TABLE public.accounts OWNER TO postgres;").unwrap();
        assert_eq!(cmd.kind, AlterOwnerKind::Table);
        assert!(!cmd.if_exists);
        assert_eq!(cmd.name.0.len(), 2);
        assert_eq!(cmd.new_owner, "postgres");
    }

    #[test]
    fn parse_alter_table_owner_if_exists_only() {
        let cmd =
            parse_alter_owner_sql("ALTER TABLE IF EXISTS ONLY public.t OWNER TO \"Alice\";")
                .unwrap();
        assert_eq!(cmd.kind, AlterOwnerKind::Table);
        assert!(cmd.if_exists);
        assert_eq!(cmd.name.0.len(), 2);
        assert_eq!(cmd.new_owner, "Alice");
    }

    #[test]
    fn parse_alter_sequence_owner() {
        let cmd = parse_alter_owner_sql("ALTER SEQUENCE s OWNER TO Alice").unwrap();
        assert_eq!(cmd.kind, AlterOwnerKind::Sequence);
        assert_eq!(cmd.name.0.len(), 1);
        assert_eq!(cmd.new_owner, "alice");
    }

    #[test]
    fn parse_alter_function_owner() {
        let cmd =
            parse_alter_owner_sql("ALTER FUNCTION public.uuidv7() OWNER TO postgres;").unwrap();
        assert_eq!(cmd.kind, AlterOwnerKind::Function);
        assert_eq!(cmd.name.0.len(), 2);
        assert_eq!(cmd.new_owner, "postgres");
    }

    #[test]
    fn parse_alter_function_owner_with_nested_parens() {
        let cmd = parse_alter_owner_sql(
            "ALTER FUNCTION public.f(numeric(10,2), text) OWNER TO postgres;",
        )
        .unwrap();
        assert_eq!(cmd.kind, AlterOwnerKind::Function);
        assert_eq!(cmd.new_owner, "postgres");
    }
}

