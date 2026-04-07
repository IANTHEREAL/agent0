//! Parser for `ALTER TYPE` enum subcommands.
//!
//! Supported syntax:
//! - `ALTER TYPE <name> RENAME TO <new_name>`
//! - `ALTER TYPE <name> RENAME VALUE '<old>' TO '<new>'`
//! - `ALTER TYPE <name> ADD VALUE [IF NOT EXISTS] '<label>' [BEFORE|AFTER '<ref>']`
//!
//! Uses the same manual-parsing approach as `alter_owner.rs`.
//!
//! The type name token is stored as a **raw substring** preserving any quote
//! characters so that the executor can pass it directly to
//! `parse_object_name_token`, which correctly handles `QuoteStyle` for
//! case-sensitive resolution.

use anyhow::{anyhow, Result};

/// Parsed representation of an ALTER TYPE subcommand.
///
/// `type_name` carries the **raw SQL token** (e.g. `"Role"`, `public."Role"`,
/// `role`) so the executor can reconstruct a proper `ObjectName` with correct
/// `QuoteStyle` via `parse_object_name_token`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AlterTypeCommand {
    /// `ALTER TYPE <name> RENAME TO <new_name>`
    RenameType { type_name: String, new_name: String },
    /// `ALTER TYPE <name> RENAME VALUE '<old>' TO '<new>'`
    RenameValue {
        type_name: String,
        old_label: String,
        new_label: String,
    },
    /// `ALTER TYPE <name> ADD VALUE [IF NOT EXISTS] '<label>' [BEFORE|AFTER '<ref>']`
    AddValue {
        type_name: String,
        if_not_exists: bool,
        new_label: String,
        position: AddValuePosition,
    },
}

impl AlterTypeCommand {
    pub(crate) fn type_name(&self) -> &str {
        match self {
            AlterTypeCommand::RenameType { type_name, .. }
            | AlterTypeCommand::RenameValue { type_name, .. }
            | AlterTypeCommand::AddValue { type_name, .. } => type_name,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AddValuePosition {
    End,
    Before(String),
    After(String),
}

// ── Parsing helpers ─────────────────────────────────────────────────

fn skip_ws(input: &str, idx: &mut usize) {
    loop {
        while *idx < input.len() {
            let ch = input[*idx..].chars().next().unwrap();
            if ch.is_whitespace() {
                *idx += ch.len_utf8();
            } else {
                break;
            }
        }

        if *idx >= input.len() {
            return;
        }

        // Line comment: -- ...\n
        if input[*idx..].starts_with("--") {
            if let Some(pos) = input[*idx..].find('\n') {
                *idx += pos + 1;
                continue;
            }
            *idx = input.len();
            return;
        }

        // Block comment: /* ... */
        if input[*idx..].starts_with("/*") {
            if let Some(pos) = input[*idx + 2..].find("*/") {
                *idx += pos + 4; // skip "/*" + body + "*/"
                continue;
            }
            *idx = input.len();
            return;
        }

        return;
    }
}

fn is_ident_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '_' || ch == '$'
}

fn consume_keyword(input: &str, idx: &mut usize, keyword: &str) -> bool {
    skip_ws(input, idx);
    let rest = &input[*idx..];
    if rest.len() < keyword.len()
        || !rest.as_bytes()[..keyword.len()].eq_ignore_ascii_case(keyword.as_bytes())
    {
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

/// Parse a possibly-schema-qualified name token and return the **raw
/// substring** from the input, preserving any double-quote characters.
///
/// Accepts: `role`, `"Role"`, `public.role`, `"MySchema"."Role"`.
///
/// The caller passes this raw token to `parse_object_name_token` (in the
/// executor) which builds `ObjectName` with correct `Ident.quote_style`.
fn parse_raw_name_token(input: &str, idx: &mut usize) -> Result<String> {
    skip_ws(input, idx);
    let start = *idx;
    if *idx >= input.len() {
        return Err(anyhow!("Expected type name"));
    }

    // Consume one or two dot-separated identifiers, each possibly quoted.
    skip_one_ident(input, idx)?;

    // Check for schema-qualified dot separator.
    if *idx < input.len() && input[*idx..].starts_with('.') {
        *idx += 1;
        skip_one_ident(input, idx)?;
    }

    Ok(input[start..*idx].to_string())
}

/// Advance `idx` past exactly one identifier (quoted or unquoted).
fn skip_one_ident(input: &str, idx: &mut usize) -> Result<()> {
    if *idx >= input.len() {
        return Err(anyhow!("Expected identifier"));
    }
    let ch = input[*idx..].chars().next().unwrap();
    if ch == '"' {
        // Quoted identifier — skip to closing quote (handling "" escapes).
        *idx += 1;
        loop {
            if *idx >= input.len() {
                return Err(anyhow!("Unterminated quoted identifier"));
            }
            let next = input[*idx..].chars().next().unwrap();
            *idx += next.len_utf8();
            if next == '"' {
                if input[*idx..].starts_with('"') {
                    *idx += 1; // escaped ""
                } else {
                    break;
                }
            }
        }
        Ok(())
    } else if is_ident_char(ch) {
        while *idx < input.len() {
            let next = input[*idx..].chars().next().unwrap();
            if is_ident_char(next) {
                *idx += next.len_utf8();
            } else {
                break;
            }
        }
        Ok(())
    } else {
        Err(anyhow!("Expected identifier"))
    }
}

/// Parse a simple (non-schema-qualified) identifier for `RENAME TO` targets.
/// Returns the resolved name: case-folded if unquoted, preserved if quoted.
/// Rejects dots — PostgreSQL does not accept schema-qualified names in
/// `ALTER TYPE ... RENAME TO`.
fn parse_simple_ident(input: &str, idx: &mut usize) -> Result<String> {
    skip_ws(input, idx);
    if *idx >= input.len() {
        return Err(anyhow!("Expected identifier"));
    }

    let ch = input[*idx..].chars().next().unwrap();
    if ch == '"' {
        // Quoted identifier — preserve case.
        *idx += 1;
        let mut out = String::new();
        loop {
            if *idx >= input.len() {
                return Err(anyhow!("Unterminated quoted identifier"));
            }
            let next = input[*idx..].chars().next().unwrap();
            *idx += next.len_utf8();
            if next == '"' {
                if input[*idx..].starts_with('"') {
                    *idx += 1;
                    out.push('"');
                } else {
                    break;
                }
            } else {
                out.push(next);
            }
        }
        Ok(out)
    } else if is_ident_char(ch) {
        let start = *idx;
        while *idx < input.len() {
            let next = input[*idx..].chars().next().unwrap();
            if is_ident_char(next) {
                *idx += next.len_utf8();
            } else {
                break;
            }
        }
        Ok(input[start..*idx].to_ascii_lowercase())
    } else {
        Err(anyhow!("Expected identifier"))
    }
}

/// Parse a single-quoted string literal (with '' escaping).
fn parse_string_literal(input: &str, idx: &mut usize) -> Result<String> {
    skip_ws(input, idx);
    if *idx >= input.len() || !input[*idx..].starts_with('\'') {
        return Err(anyhow!("Expected string literal"));
    }
    *idx += 1; // skip opening quote

    let mut out = String::new();
    loop {
        if *idx >= input.len() {
            return Err(anyhow!("Unterminated string literal"));
        }
        let ch = input[*idx..].chars().next().unwrap();
        *idx += ch.len_utf8();
        if ch == '\'' {
            if input[*idx..].starts_with('\'') {
                *idx += 1;
                out.push('\'');
            } else {
                break;
            }
        } else {
            out.push(ch);
        }
    }
    Ok(out)
}

/// Ensure the parser consumed the entire input. Any remaining non-whitespace
/// is a syntax error (trailing garbage).
fn expect_end(input: &str, idx: &mut usize) -> Result<()> {
    skip_ws(input, idx);
    if *idx < input.len() {
        Err(anyhow!(
            "syntax error at or near \"{}\"",
            input[*idx..].split_whitespace().next().unwrap_or("")
        ))
    } else {
        Ok(())
    }
}

// ── Public entry point ──────────────────────────────────────────────

pub(crate) fn parse_alter_type_sql(sql: &str) -> Result<AlterTypeCommand> {
    let sql = super::executor::triggers::strip_leading_sql_comments(sql).trim();
    let sql = sql.trim_end_matches(';').trim_end();

    let mut idx = 0usize;
    expect_keyword(sql, &mut idx, "ALTER")?;
    expect_keyword(sql, &mut idx, "TYPE")?;

    let type_name = parse_raw_name_token(sql, &mut idx)?;

    if consume_keyword(sql, &mut idx, "RENAME") {
        if consume_keyword(sql, &mut idx, "VALUE") {
            // ALTER TYPE <name> RENAME VALUE '<old>' TO '<new>'
            let old_label = parse_string_literal(sql, &mut idx)?;
            expect_keyword(sql, &mut idx, "TO")?;
            let new_label = parse_string_literal(sql, &mut idx)?;
            expect_end(sql, &mut idx)?;
            return Ok(AlterTypeCommand::RenameValue {
                type_name,
                old_label,
                new_label,
            });
        }
        // ALTER TYPE <name> RENAME TO <new_name>
        expect_keyword(sql, &mut idx, "TO")?;
        let new_name = parse_simple_ident(sql, &mut idx)?;
        expect_end(sql, &mut idx)?;
        return Ok(AlterTypeCommand::RenameType {
            type_name,
            new_name,
        });
    }

    if consume_keyword(sql, &mut idx, "ADD") {
        expect_keyword(sql, &mut idx, "VALUE")?;

        let if_not_exists = if consume_keyword(sql, &mut idx, "IF") {
            expect_keyword(sql, &mut idx, "NOT")?;
            expect_keyword(sql, &mut idx, "EXISTS")?;
            true
        } else {
            false
        };

        let new_label = parse_string_literal(sql, &mut idx)?;

        let position = if consume_keyword(sql, &mut idx, "BEFORE") {
            AddValuePosition::Before(parse_string_literal(sql, &mut idx)?)
        } else if consume_keyword(sql, &mut idx, "AFTER") {
            AddValuePosition::After(parse_string_literal(sql, &mut idx)?)
        } else {
            AddValuePosition::End
        };

        expect_end(sql, &mut idx)?;

        return Ok(AlterTypeCommand::AddValue {
            type_name,
            if_not_exists,
            new_label,
            position,
        });
    }

    Err(anyhow!(
        "Unsupported ALTER TYPE subcommand. Supported: RENAME TO, RENAME VALUE, ADD VALUE"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_rename_to() {
        let cmd = parse_alter_type_sql("ALTER TYPE role RENAME TO new_role;").unwrap();
        assert_eq!(
            cmd,
            AlterTypeCommand::RenameType {
                type_name: "role".to_string(),
                new_name: "new_role".to_string(),
            }
        );
    }

    #[test]
    fn parse_rename_to_quoted() {
        let cmd = parse_alter_type_sql("ALTER TYPE \"Role\" RENAME TO \"NewRole\"").unwrap();
        assert_eq!(
            cmd,
            AlterTypeCommand::RenameType {
                type_name: "\"Role\"".to_string(),
                new_name: "NewRole".to_string(),
            }
        );
    }

    #[test]
    fn parse_rename_value() {
        let cmd = parse_alter_type_sql("ALTER TYPE role RENAME VALUE 'USER' TO 'MEMBER';").unwrap();
        assert_eq!(
            cmd,
            AlterTypeCommand::RenameValue {
                type_name: "role".to_string(),
                old_label: "USER".to_string(),
                new_label: "MEMBER".to_string(),
            }
        );
    }

    #[test]
    fn parse_rename_value_escaped_quotes() {
        let cmd =
            parse_alter_type_sql("ALTER TYPE role RENAME VALUE 'O''Brien' TO 'O''Reilly'").unwrap();
        assert_eq!(
            cmd,
            AlterTypeCommand::RenameValue {
                type_name: "role".to_string(),
                old_label: "O'Brien".to_string(),
                new_label: "O'Reilly".to_string(),
            }
        );
    }

    #[test]
    fn parse_add_value_at_end() {
        let cmd = parse_alter_type_sql("ALTER TYPE role ADD VALUE 'MODERATOR'").unwrap();
        assert_eq!(
            cmd,
            AlterTypeCommand::AddValue {
                type_name: "role".to_string(),
                if_not_exists: false,
                new_label: "MODERATOR".to_string(),
                position: AddValuePosition::End,
            }
        );
    }

    #[test]
    fn parse_add_value_if_not_exists() {
        let cmd =
            parse_alter_type_sql("ALTER TYPE role ADD VALUE IF NOT EXISTS 'MODERATOR';").unwrap();
        assert_eq!(
            cmd,
            AlterTypeCommand::AddValue {
                type_name: "role".to_string(),
                if_not_exists: true,
                new_label: "MODERATOR".to_string(),
                position: AddValuePosition::End,
            }
        );
    }

    #[test]
    fn parse_add_value_before() {
        let cmd =
            parse_alter_type_sql("ALTER TYPE role ADD VALUE 'MODERATOR' BEFORE 'ADMIN'").unwrap();
        assert_eq!(
            cmd,
            AlterTypeCommand::AddValue {
                type_name: "role".to_string(),
                if_not_exists: false,
                new_label: "MODERATOR".to_string(),
                position: AddValuePosition::Before("ADMIN".to_string()),
            }
        );
    }

    #[test]
    fn parse_add_value_after() {
        let cmd =
            parse_alter_type_sql("ALTER TYPE role ADD VALUE 'MODERATOR' AFTER 'USER'").unwrap();
        assert_eq!(
            cmd,
            AlterTypeCommand::AddValue {
                type_name: "role".to_string(),
                if_not_exists: false,
                new_label: "MODERATOR".to_string(),
                position: AddValuePosition::After("USER".to_string()),
            }
        );
    }

    #[test]
    fn parse_schema_qualified() {
        let cmd = parse_alter_type_sql("ALTER TYPE public.role ADD VALUE 'MODERATOR'").unwrap();
        assert_eq!(
            cmd,
            AlterTypeCommand::AddValue {
                type_name: "public.role".to_string(),
                if_not_exists: false,
                new_label: "MODERATOR".to_string(),
                position: AddValuePosition::End,
            }
        );
    }

    #[test]
    fn parse_schema_qualified_quoted() {
        let cmd = parse_alter_type_sql("ALTER TYPE \"MySchema\".\"Role\" ADD VALUE 'X'").unwrap();
        assert_eq!(
            cmd,
            AlterTypeCommand::AddValue {
                type_name: "\"MySchema\".\"Role\"".to_string(),
                if_not_exists: false,
                new_label: "X".to_string(),
                position: AddValuePosition::End,
            }
        );
    }

    #[test]
    fn parse_unsupported_subcommand() {
        let err = parse_alter_type_sql("ALTER TYPE role DROP VALUE 'USER'").unwrap_err();
        assert!(err
            .to_string()
            .contains("Unsupported ALTER TYPE subcommand"));
    }

    // P1-1: trailing garbage must be rejected.
    #[test]
    fn parse_rejects_trailing_garbage() {
        let err = parse_alter_type_sql("ALTER TYPE color ADD VALUE 'blue' AFTER 'green' EXTRA")
            .unwrap_err();
        assert!(err.to_string().contains("syntax error"));
    }

    // P0-3: RENAME TO must reject dotted names.
    #[test]
    fn parse_rename_to_rejects_dotted_name() {
        let err = parse_alter_type_sql("ALTER TYPE color RENAME TO public.palette").unwrap_err();
        assert!(err.to_string().contains("syntax error"));
    }

    #[test]
    fn parse_add_value_accepts_trailing_line_comment() {
        let cmd =
            parse_alter_type_sql("ALTER TYPE color ADD VALUE 'blue' -- trailing comment").unwrap();
        assert_eq!(
            cmd,
            AlterTypeCommand::AddValue {
                type_name: "color".to_string(),
                if_not_exists: false,
                new_label: "blue".to_string(),
                position: AddValuePosition::End,
            }
        );
    }

    #[test]
    fn parse_add_value_accepts_inline_block_comment() {
        let cmd =
            parse_alter_type_sql("ALTER TYPE color ADD /*c*/ VALUE 'blue' /*x*/ AFTER 'green'")
                .unwrap();
        assert_eq!(
            cmd,
            AlterTypeCommand::AddValue {
                type_name: "color".to_string(),
                if_not_exists: false,
                new_label: "blue".to_string(),
                position: AddValuePosition::After("green".to_string()),
            }
        );
    }
}
