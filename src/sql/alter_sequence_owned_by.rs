use super::helpers::normalize_ident;
use anyhow::{anyhow, Result};
use sqlparser::ast::{Ident, ObjectName};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct AlterSequenceOwnedByCommand {
    pub(crate) if_exists: bool,
    pub(crate) sequence_name: ObjectName,
    /// `None` represents `OWNED BY NONE`.
    pub(crate) owned_by: Option<(ObjectName, String)>,
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

fn parse_owned_by_target(input: &str, idx: &mut usize) -> Result<Option<(ObjectName, String)>> {
    if consume_keyword(input, idx, "NONE") {
        return Ok(None);
    }

    let first = parse_ident(input, idx)?;
    skip_ws(input, idx);
    if !input[*idx..].starts_with('.') {
        return Err(anyhow!("OWNED BY target must be <table>.<column>"));
    }
    *idx += 1;
    let second = parse_ident(input, idx)?;
    skip_ws(input, idx);

    // Two-part form: table.column
    if !input[*idx..].starts_with('.') {
        let table = ObjectName(vec![first]);
        return Ok(Some((table, normalize_ident(&second))));
    }

    // Three-part form: schema.table.column
    *idx += 1;
    let third = parse_ident(input, idx)?;
    skip_ws(input, idx);
    if input[*idx..].starts_with('.') {
        return Err(anyhow!("OWNED BY target must be at most schema.table.column"));
    }

    Ok(Some((
        ObjectName(vec![first, second]),
        normalize_ident(&third),
    )))
}

pub(crate) fn parse_alter_sequence_owned_by_sql(sql: &str) -> Result<AlterSequenceOwnedByCommand> {
    let sql = super::executor_functions_triggers::strip_leading_sql_comments(sql).trim();
    let sql = sql.trim_end_matches(';').trim_end();

    let mut idx = 0usize;
    expect_keyword(sql, &mut idx, "ALTER")?;
    expect_keyword(sql, &mut idx, "SEQUENCE")?;

    let if_exists = if consume_keyword(sql, &mut idx, "IF") {
        expect_keyword(sql, &mut idx, "EXISTS")?;
        true
    } else {
        false
    };

    let sequence_name = parse_object_name_max2(sql, &mut idx)?;

    expect_keyword(sql, &mut idx, "OWNED")?;
    expect_keyword(sql, &mut idx, "BY")?;
    let owned_by = parse_owned_by_target(sql, &mut idx)?;

    Ok(AlterSequenceOwnedByCommand {
        if_exists,
        sequence_name,
        owned_by,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_owned_by_schema_table_column() {
        let cmd = parse_alter_sequence_owned_by_sql(
            "ALTER SEQUENCE public.invitation_codes_id_seq OWNED BY public.invitation_codes.id;",
        )
        .unwrap();
        assert!(!cmd.if_exists);
        assert_eq!(cmd.sequence_name.0.len(), 2);
        let (tbl, col) = cmd.owned_by.expect("expected owned-by target");
        assert_eq!(tbl.0.len(), 2);
        assert_eq!(col, "id");
    }

    #[test]
    fn parse_owned_by_table_column_if_exists() {
        let cmd = parse_alter_sequence_owned_by_sql("ALTER SEQUENCE IF EXISTS s OWNED BY t.id")
            .unwrap();
        assert!(cmd.if_exists);
        assert_eq!(cmd.sequence_name.0.len(), 1);
        let (tbl, col) = cmd.owned_by.expect("expected owned-by target");
        assert_eq!(tbl.0.len(), 1);
        assert_eq!(normalize_ident(&tbl.0[0]), "t");
        assert_eq!(col, "id");
    }

    #[test]
    fn parse_owned_by_none() {
        let cmd = parse_alter_sequence_owned_by_sql("ALTER SEQUENCE s OWNED BY NONE").unwrap();
        assert!(cmd.owned_by.is_none());
    }
}

